use crate::{config::Proxy, web::Link};
use axum::{
    body::Body,
    extract::Request,
    http::{header as h, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use http_body::Body as _;
use std::{
    net::SocketAddr,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

pub fn strip_hop(hdr: &mut HeaderMap) {
    let named: Vec<HeaderName> = hdr
        .get_all(h::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .filter_map(|s| HeaderName::from_bytes(s.trim().as_bytes()).ok())
        .collect();
    for n in named {
        hdr.remove(n);
    }
    for n in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        hdr.remove(n);
    }
}
pub fn clean_client_headers(hdr: &mut HeaderMap) {
    strip_hop(hdr);
    let names: Vec<HeaderName> = hdr
        .keys()
        .filter(|k| {
            k.as_str().starts_with("x-forwarded-")
                || matches!(k.as_str(), "forwarded" | "x-real-ip" | "proxy" | "expect")
        })
        .cloned()
        .collect();
    for n in names {
        hdr.remove(n);
    }
}
pub fn host<'a>(uri: &'a axum::http::Uri, hdr: &'a HeaderMap) -> Option<&'a str> {
    uri.authority()
        .map(|a| a.as_str())
        .or_else(|| hdr.get(h::HOST)?.to_str().ok())
}

pub async fn forward(
    client: &reqwest::Client,
    counter: &AtomicUsize,
    cfg: &Proxy,
    peer: SocketAddr,
    link: Link,
    req: Request,
) -> Response {
    if req.method() == Method::CONNECT || req.headers().contains_key(h::UPGRADE) {
        return StatusCode::NOT_IMPLEMENTED.into_response();
    }
    let i = counter.fetch_add(1, Ordering::Relaxed) % cfg.upstreams.len();
    let (mut parts, body) = req.into_parts();
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    let url = format!("{}{path}", cfg.upstreams[i]);
    let original_host =
        host(&parts.uri, &parts.headers).and_then(|s| HeaderValue::from_str(s).ok());
    clean_client_headers(&mut parts.headers);
    parts.headers.remove(h::HOST);
    if let Some(v) = original_host {
        parts.headers.insert("x-forwarded-host", v);
    }
    parts.headers.insert(
        "x-forwarded-for",
        peer.ip().to_string().parse().expect("IP"),
    );
    parts
        .headers
        .insert("x-forwarded-proto", HeaderValue::from_static(link.scheme()));
    let head = parts.method == Method::HEAD;
    let mut request = client
        .request(parts.method, url)
        .headers(parts.headers)
        .timeout(Duration::from_secs(cfg.timeout_secs));
    if !body.is_end_stream() {
        request = request.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    }
    let mut res = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            return if e.is_timeout() {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            }
            .into_response()
        }
    };
    let code = res.status();
    if code.is_informational() {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let mut headers = std::mem::take(res.headers_mut());
    strip_hop(&mut headers);
    let body = if head || code == StatusCode::NO_CONTENT || code == StatusCode::NOT_MODIFIED {
        Body::empty()
    } else {
        Body::from_stream(res.bytes_stream())
    };
    let mut r = Response::new(body);
    *r.status_mut() = code;
    *r.headers_mut() = headers;
    r
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn connection_options_are_not_forwarded() {
        let mut h = HeaderMap::new();
        h.append("connection", "keep-alive, X-Internal".parse().unwrap());
        h.append("connection", "Other".parse().unwrap());
        for n in [
            "keep-alive",
            "x-internal",
            "other",
            "x-forwarded-user",
            "forwarded",
            "cookie",
        ] {
            h.insert(n, "value".parse().unwrap());
        }
        clean_client_headers(&mut h);
        assert_eq!(h.len(), 1);
        assert!(h.contains_key("cookie"));
    }
}
