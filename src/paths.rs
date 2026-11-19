use axum::http::StatusCode;
use percent_encoding::percent_decode_str;
use std::borrow::Cow;

pub fn decode(raw: &str) -> Result<Cow<'_, str>, StatusCode> {
    if raw.len() > 8192 {
        return Err(StatusCode::URI_TOO_LONG);
    }
    if !raw.starts_with('/') || raw.contains("//") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                return Err(StatusCode::BAD_REQUEST);
            }
            let hi = (b[i + 1] as char)
                .to_digit(16)
                .ok_or(StatusCode::BAD_REQUEST)?;
            let lo = (b[i + 2] as char)
                .to_digit(16)
                .ok_or(StatusCode::BAD_REQUEST)?;
            if matches!((hi * 16 + lo) as u8, b'/' | b'\\' | b'%') {
                return Err(StatusCode::BAD_REQUEST);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    let path = percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    if path.split('/').any(|s| !s.is_empty() && !public_segment(s)) {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(path)
}

pub fn public_segment(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && !s.ends_with(['.', ' ', '~'])
        && !s.contains(['/', '\\', ':', '%', '#', '?'])
        && !s.chars().any(char::is_control)
}
fn ext(s: &str) -> &str {
    s.rsplit_once('.').map(|v| v.1).unwrap_or("")
}
pub fn is_php(s: &str) -> bool {
    ext(s).eq_ignore_ascii_case("php")
}
pub fn reserved_script(s: &str) -> bool {
    s.split('/').flat_map(|p| p.split('.').skip(1)).any(|p| {
        [
            "php", "phtml", "phps", "phar", "inc", "php3", "php4", "php5", "php7", "php8",
        ]
        .iter()
        .any(|e| p.eq_ignore_ascii_case(e))
    })
}
pub fn mime(name: &str) -> Option<&'static str> {
    if reserved_script(name) {
        return None;
    }
    let ext = ext(name).to_ascii_lowercase();
    Some(match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        "avif" => "image/avif",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "png" | "gif" | "jpg" | "jpeg" | "ico" | "svg" | "bmp" | "apng" | "mp3" | "mp4" | "m4a"
        | "ogg" | "opus" | "wav" | "webm" | "pdf" | "xml" | "csv" | "ttf" | "otf" | "vtt"
        | "zip" | "bin" => mime_guess::from_ext(&ext)
            .first_raw()
            .unwrap_or("application/octet-stream"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn url_namespace() {
        assert_eq!(decode("/a%20b/app.mjs").unwrap(), "/a b/app.mjs");
        for p in [
            "/.env",
            "/a/%2e%2e/b",
            "/a%2fb",
            "/%00",
            "/%zz",
            "/%",
            "//x",
            "/a%5cb",
            "/a%252fb",
            "/a:b",
        ] {
            assert!(decode(p).is_err(), "{p}");
        }
    }
    #[test]
    fn source_is_not_an_asset() {
        for n in [
            "x.PHP",
            "test.php.txt",
            "x.phtml",
            "backup.php~",
            "key.pem",
            "server.cjs",
        ] {
            assert!(mime(n).is_none());
        }
        assert_eq!(mime("mod.WASM"), Some("application/wasm"));
    }
}
