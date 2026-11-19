use crate::{config::Config, paths};
use anyhow::{ensure, Result};
use axum::{
    body::Body,
    http::{header as h, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read, SeekFrom},
    sync::Arc,
    time::UNIX_EPOCH,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

pub fn open_regular(dir: &Dir, name: &str) -> io::Result<File> {
    open_regular_with_meta(dir, name).map(|(f, _)| f)
}
fn open_regular_with_meta(dir: &Dir, name: &str) -> io::Result<(File, std::fs::Metadata)> {
    let mut o = OpenOptions::new();
    o.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_fs_ext::OpenOptionsExt;
        o.custom_flags(libc::O_NONBLOCK);
    }
    let f = dir.open_with(name, &o)?.into_std();
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok((f, meta))
}
pub async fn reopen(root: Arc<Dir>, url: &str) -> io::Result<(File, std::fs::Metadata)> {
    let relative = url.trim_start_matches('/').to_owned();
    tokio::task::spawn_blocking(move || {
        let mut parts = relative.split('/').peekable();
        let mut at: Option<Dir> = None;
        while let Some(part) = parts.next() {
            let d = at.as_ref().unwrap_or(root.as_ref());
            if parts.peek().is_none() {
                return open_regular_with_meta(d, part);
            }
            at = Some(d.open_dir_nofollow(part)?);
        }
        Err(io::ErrorKind::NotFound.into())
    })
    .await
    .map_err(io::Error::other)?
}

pub enum Asset {
    Memory {
        data: Bytes,
        tag: HeaderValue,
        mime: &'static str,
    },
    Disk(&'static str),
    Php,
}
pub enum Found<'a> {
    File(&'a str, &'a Asset),
    Slash,
    Missing,
}
pub struct Catalog {
    pub root: Arc<Dir>,
    files: HashMap<String, Asset>,
    dirs: HashMap<String, Option<String>>,
    pub bytes: usize,
    visited: usize,
}
impl Catalog {
    pub fn build(cfg: &Config) -> Result<Self> {
        let root = Arc::new(Dir::open_ambient_dir(
            &cfg.web.static_dir,
            ambient_authority(),
        )?);
        let mut c = Self {
            root: root.clone(),
            files: HashMap::new(),
            dirs: HashMap::new(),
            bytes: 0,
            visited: 0,
        };
        c.scan(&root, "/", 0, cfg)?;
        for (d, index) in &mut c.dirs {
            for n in [
                Some(cfg.web.default_page.as_str()),
                cfg.php.enabled.then_some("index.php"),
            ]
            .into_iter()
            .flatten()
            {
                let candidate = format!("{d}{n}");
                if c.files.contains_key(&candidate) {
                    *index = Some(candidate);
                    break;
                }
            }
        }
        Ok(c)
    }
    fn scan(&mut self, dir: &Dir, prefix: &str, depth: u8, cfg: &Config) -> Result<()> {
        ensure!(depth <= 64, "document root exceeds 64 directory levels");
        self.dirs.insert(prefix.into(), None);
        for e in dir.entries()? {
            let e = e?;
            self.visited += 1;
            ensure!(
                self.visited <= 100_000,
                "document root exceeds 100000 entries"
            );
            let Ok(name) = e.file_name().into_string() else {
                continue;
            };
            if !paths::public_segment(&name) {
                continue;
            }
            let ty = e.file_type()?;
            let url = format!("{prefix}{name}");
            if ty.is_dir() {
                if !paths::reserved_script(&name) {
                    self.scan(
                        &dir.open_dir_nofollow(&name)?,
                        &format!("{url}/"),
                        depth + 1,
                        cfg,
                    )?;
                }
                continue;
            }
            if !ty.is_file() {
                continue;
            }
            if paths::is_php(&name) {
                if cfg.php.enabled {
                    self.files.insert(url, Asset::Php);
                }
                continue;
            }
            let Some(mime) = paths::mime(&name) else {
                continue;
            };
            let remaining = (cfg.web.cache_bytes - self.bytes).min(cfg.web.cache_file_bytes);
            let asset = if remaining > 0 && e.metadata()?.len() <= remaining as u64 {
                let mut data = Vec::new();
                open_regular(dir, &name)?
                    .take(remaining as u64 + 1)
                    .read_to_end(&mut data)?;
                if data.len() <= remaining {
                    self.bytes += data.len();
                    let tag = format!("\"{}\"", blake3::hash(&data).to_hex()).parse()?;
                    Asset::Memory {
                        data: data.into(),
                        tag,
                        mime,
                    }
                } else {
                    Asset::Disk(mime)
                }
            } else {
                Asset::Disk(mime)
            };
            self.files.insert(url, asset);
        }
        Ok(())
    }
    pub fn lookup(&self, path: &str) -> Found<'_> {
        if let Some((k, v)) = self.files.get_key_value(path) {
            return Found::File(k, v);
        }
        if path.ends_with('/') {
            if let Some(Some(index)) = self.dirs.get(path) {
                if let Some(v) = self.files.get(index) {
                    return Found::File(index, v);
                }
            }
        } else if self.dirs.contains_key(&format!("{path}/")) {
            return Found::Slash;
        }
        Found::Missing
    }
}

enum Slice {
    Full,
    Part(u64, u64),
    Unsatisfiable,
}
fn range(value: &str, len: u64) -> Slice {
    let Some(r) = value.strip_prefix("bytes=") else {
        return Slice::Full;
    };
    let Some((a, b)) = r.split_once('-') else {
        return Slice::Full;
    };
    let number = |s: &str| {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            None
        } else {
            s.parse::<u64>().ok()
        }
    };
    if a.is_empty() {
        let Some(n) = number(b) else {
            return Slice::Full;
        };
        if n == 0 || len == 0 {
            Slice::Unsatisfiable
        } else {
            Slice::Part(len.saturating_sub(n), len - 1)
        }
    } else {
        let Some(start) = number(a) else {
            return Slice::Full;
        };
        let end = if b.is_empty() {
            u64::MAX
        } else if let Some(n) = number(b) {
            n
        } else {
            return Slice::Full;
        };
        if end < start {
            return Slice::Full;
        }
        if start >= len {
            Slice::Unsatisfiable
        } else {
            Slice::Part(start, end.min(len - 1))
        }
    }
}
fn method_error() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, [(h::ALLOW, "GET, HEAD")]).into_response()
}
fn weak(s: &str) -> &str {
    s.strip_prefix("W/").unwrap_or(s)
}
fn matches_etag(headers: &HeaderMap, tag: Option<&HeaderValue>) -> bool {
    let tag = tag.and_then(|t| t.to_str().ok()).map(weak);
    headers
        .get_all(h::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|v| v == "*" || Some(weak(v)) == tag)
}

pub async fn serve(
    root: &Arc<Dir>,
    url: &str,
    asset: &Asset,
    method: &Method,
    headers: &HeaderMap,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return method_error();
    }
    let (len, tag, mime, mut file, data) = match asset {
        Asset::Memory { data, tag, mime } => (
            data.len() as u64,
            Some(tag.clone()),
            *mime,
            None,
            Some(data),
        ),
        Asset::Disk(mime) => {
            let Ok((f, m)) = reopen(root.clone(), url).await else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let f = tokio::fs::File::from_std(f);
            let tag = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .and_then(|d| {
                    format!("W/\"{:x}-{:x}\"", m.len(), d.as_nanos())
                        .parse()
                        .ok()
                });
            (m.len(), tag, *mime, Some(f), None)
        }
        Asset::Php => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut r = Response::new(Body::empty());
    let hdr = r.headers_mut();
    hdr.insert(h::CONTENT_TYPE, HeaderValue::from_static(mime));
    hdr.insert(h::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    hdr.insert(h::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if let Some(t) = &tag {
        hdr.insert(h::ETAG, t.clone());
    }
    if matches_etag(headers, tag.as_ref()) {
        *r.status_mut() = StatusCode::NOT_MODIFIED;
        return r;
    }
    let permitted = headers
        .get(h::IF_RANGE)
        .is_none_or(|v| !v.as_bytes().starts_with(b"W/") && Some(v) == tag.as_ref());
    let sel = if method == Method::GET && permitted {
        headers
            .get(h::RANGE)
            .and_then(|v| v.to_str().ok())
            .map_or(Slice::Full, |s| range(s, len))
    } else {
        Slice::Full
    };
    let (start, count) = match sel {
        Slice::Full => (0, len),
        Slice::Part(a, b) => {
            *r.status_mut() = StatusCode::PARTIAL_CONTENT;
            r.headers_mut().insert(
                h::CONTENT_RANGE,
                format!("bytes {a}-{b}/{len}")
                    .parse()
                    .expect("numeric range"),
            );
            (a, b - a + 1)
        }
        Slice::Unsatisfiable => {
            *r.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            r.headers_mut().insert(
                h::CONTENT_RANGE,
                format!("bytes */{len}").parse().expect("numeric range"),
            );
            return r;
        }
    };
    r.headers_mut()
        .insert(h::CONTENT_LENGTH, HeaderValue::from(count));
    if method == Method::HEAD || count == 0 {
        return r;
    }
    if let Some(data) = data {
        *r.body_mut() = Body::from(data.slice(start as usize..(start + count) as usize));
    } else if let Some(mut f) = file.take() {
        if start > 0 && f.seek(SeekFrom::Start(start)).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        *r.body_mut() = Body::from_stream(ReaderStream::with_capacity(f.take(count), 64 * 1024));
    }
    r
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_ranges() {
        assert!(matches!(range("bytes=-3", 8), Slice::Part(5, 7)));
        assert!(matches!(range("bytes=4-", 8), Slice::Part(4, 7)));
        assert!(matches!(range("bytes=0-99", 8), Slice::Part(0, 7)));
        assert!(matches!(range("bytes=8-", 8), Slice::Unsatisfiable));
        assert!(matches!(range("bytes=0-0", 0), Slice::Unsatisfiable));
        assert!(matches!(range("bytes=0-1,3-4", 9), Slice::Full));
    }
}
