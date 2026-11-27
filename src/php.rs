use crate::{
    assets, proxy,
    web::{Link, Snapshot},
};
use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::{header as h, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::{Bytes, BytesMut};
use fastcgi_client::{response::Content, Client, Params, Request as CgiRequest};
use futures_util::{Stream, StreamExt};
use std::{error::Error as _, io, net::SocketAddr, sync::Arc};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::OwnedSemaphorePermit,
    time::{timeout_at, Instant},
};

const HEADER_LIMIT: usize = 32 * 1024;
trait Socket: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Socket for T {}
async fn connect(endpoint: &str) -> io::Result<Box<dyn Socket>> {
    #[cfg(unix)]
    if let Some(path) = endpoint.strip_prefix("unix:") {
        return Ok(Box::new(tokio::net::UnixStream::connect(path).await?));
    }
    let s = TcpStream::connect(endpoint).await?;
    s.set_nodelay(true)?;
    Ok(Box::new(s))
}

pub async fn serve(
    site: &Snapshot,
    script: &str,
    peer: SocketAddr,
    link: Link,
    req: Request,
) -> Response {
    let cfg = &site.cfg.php;
    let Ok(slot) = Arc::clone(&site.php_slots).try_acquire_owned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, [(h::RETRY_AFTER, "1")]).into_response();
    };
    let until = Instant::now() + cfg.timeout();
    match timeout_at(until, run(site, script, peer, link, req, slot, until)).await {
        Ok(Ok(r)) => r,
        Ok(Err(s)) => s.into_response(),
        Err(_) => StatusCode::GATEWAY_TIMEOUT.into_response(),
    }
}
async fn run(
    site: &Snapshot,
    script: &str,
    peer: SocketAddr,
    link: Link,
    req: Request,
    slot: OwnedSemaphorePermit,
    until: Instant,
) -> Result<Response, StatusCode> {
    let cfg = &site.cfg.php;
    let _checked = assets::reopen(site.catalog.root.clone(), script)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let (mut parts, body) = req.into_parts();
    if parts
        .headers
        .get(h::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .is_some_and(|n| n > cfg.max_body_bytes as u64)
    {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    proxy::clean_client_headers(&mut parts.headers);
    let b = to_bytes(body, cfg.max_body_bytes).await.map_err(|e| {
        if e.source()
            .is_some_and(|v| v.is::<http_body_util::LengthLimitError>())
        {
            StatusCode::PAYLOAD_TOO_LARGE
        } else {
            StatusCode::BAD_REQUEST
        }
    })?;
    let root = &site.cfg.web.static_dir;
    let filename = root.join(&script[1..]);
    let host = proxy::host(&parts.uri, &parts.headers).unwrap_or("localhost");
    let authority = host
        .parse::<axum::http::uri::Authority>()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let mut p = Params::default()
        .gateway_interface("CGI/1.1")
        .server_software("eafrei")
        .server_protocol(format!("{:?}", parts.version))
        .request_method(parts.method.as_str())
        .request_uri(parts.uri.path_and_query().map_or("/", |p| p.as_str()))
        .query_string(parts.uri.query().unwrap_or(""))
        .document_root(root.to_str().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?)
        .script_filename(filename.to_str().ok_or(StatusCode::INTERNAL_SERVER_ERROR)?)
        .script_name(script)
        .document_uri(script)
        .remote_addr(peer.ip().to_string())
        .remote_port(peer.port())
        .server_name(authority.host())
        .server_port(link.addr.port())
        .server_addr(link.addr.ip().to_string())
        .content_length(b.len())
        .custom("REQUEST_SCHEME", link.scheme())
        .custom("REDIRECT_STATUS", "200")
        .custom("HTTP_HOST", host);
    if link.tls {
        p = p.custom("HTTPS", "on");
    }
    if let Some(v) = parts.headers.get(h::CONTENT_TYPE) {
        p = p.content_type(v.to_str().map_err(|_| StatusCode::BAD_REQUEST)?);
    }
    let mut header_bytes = 0;
    for name in parts.headers.keys() {
        let n = name.as_str();
        if matches!(n, "content-type" | "content-length" | "host")
            || n.eq_ignore_ascii_case("proxy")
            || !n.bytes().all(|b| b == b'-' || b.is_ascii_alphanumeric())
        {
            continue;
        }
        let env_name = n.replace('-', "_").to_ascii_uppercase();
        if env_name == "PROXY" {
            continue;
        }
        let mut val = String::new();
        for v in parts.headers.get_all(name) {
            let s = v.to_str().map_err(|_| StatusCode::BAD_REQUEST)?;
            header_bytes += n.len() + s.len();
            if header_bytes > HEADER_LIMIT {
                return Err(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
            }
            if !val.is_empty() {
                val.push_str(if n == "cookie" { "; " } else { ", " });
            }
            val.push_str(s);
        }
        p = p.custom(format!("HTTP_{env_name}"), val);
    }
    let conn = connect(&cfg.endpoint)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let mut stream = Client::new_tokio(conn)
        .execute_once_stream(CgiRequest::new(p, fastcgi_client::io::Cursor::new(b)))
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    let mut buf = BytesMut::with_capacity(4096);
    let end = loop {
        let chunk = stream
            .next()
            .await
            .ok_or(StatusCode::BAD_GATEWAY)?
            .map_err(|_| StatusCode::BAD_GATEWAY)?;
        if let Content::Stdout(bytes) = chunk {
            let from = buf.len().saturating_sub(3);
            buf.extend_from_slice(&bytes);
            if let Some(i) = buf[from..].windows(4).position(|w| w == b"\r\n\r\n") {
                let end = from + i + 4;
                if end > HEADER_LIMIT {
                    return Err(StatusCode::BAD_GATEWAY);
                }
                break end;
            }
            if buf.len() >= HEADER_LIMIT {
                return Err(StatusCode::BAD_GATEWAY);
            }
        }
    };
    let headers = buf.split_to(end);
    let (code, hdr) = parse_headers(&headers)?;
    let body = if parts.method == Method::HEAD
        || code == StatusCode::NO_CONTENT
        || code == StatusCode::NOT_MODIFIED
    {
        Body::empty()
    } else {
        Body::from_stream(body_stream(stream, buf.freeze(), slot, until))
    };
    let mut r = Response::new(body);
    *r.status_mut() = code;
    *r.headers_mut() = hdr;
    Ok(r)
}
fn parse_headers(buf: &[u8]) -> Result<(StatusCode, HeaderMap), StatusCode> {
    let bad = StatusCode::BAD_GATEWAY;
    let mut tmp = [httparse::EMPTY_HEADER; 96];
    let httparse::Status::Complete((_, parsed)) =
        httparse::parse_headers(buf, &mut tmp).map_err(|_| bad)?
    else {
        return Err(bad);
    };
    let mut status = None;
    let mut hdr = HeaderMap::new();
    for item in parsed {
        if item.name.eq_ignore_ascii_case("status") {
            if status.is_some() {
                return Err(bad);
            }
            let s = std::str::from_utf8(item.value)
                .map_err(|_| bad)?
                .split_whitespace()
                .next()
                .ok_or(bad)?;
            let code = StatusCode::from_bytes(s.as_bytes()).map_err(|_| bad)?;
            if code.is_informational() {
                return Err(bad);
            }
            status = Some(code);
        } else {
            hdr.append(
                HeaderName::from_bytes(item.name.as_bytes()).map_err(|_| bad)?,
                HeaderValue::from_bytes(item.value).map_err(|_| bad)?,
            );
        }
    }
    proxy::strip_hop(&mut hdr);
    hdr.remove("x-powered-by");
    hdr.remove(h::SERVER);
    let code = status.unwrap_or(if hdr.contains_key(h::LOCATION) {
        StatusCode::FOUND
    } else {
        StatusCode::OK
    });
    Ok((code, hdr))
}
fn body_stream<S>(
    mut stream: S,
    first: Bytes,
    slot: OwnedSemaphorePermit,
    until: Instant,
) -> impl Stream<Item = io::Result<Bytes>> + Send
where
    S: Stream<Item = Result<Content, fastcgi_client::ClientError>> + Unpin + Send + 'static,
{
    async_stream::try_stream! {let _slot=slot;if !first.is_empty(){yield first;}
        while let Some(item)=timeout_at(until,stream.next()).await.map_err(io::Error::other)? {if let Content::Stdout(b)=item.map_err(io::Error::other)? {if !b.is_empty(){yield b;}}}
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cgi_headers() {
        let (code, h) =
            parse_headers(b"Status: 201 Created\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\n\r\n")
                .unwrap();
        assert_eq!(code, 201);
        assert_eq!(h.get_all(h::SET_COOKIE).iter().count(), 2);
        assert_eq!(parse_headers(b"Location: /new\r\n\r\n").unwrap().0, 302);
        assert!(parse_headers(b"Status: abc\r\n\r\n").is_err());
    }
}
