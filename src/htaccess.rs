use anyhow::{bail, ensure, Context, Result};
use axum::http::HeaderValue;
use cap_std::fs::Dir;
use std::io::Read;
use std::{
    collections::{HashMap, HashSet},
    io::ErrorKind,
    net::IpAddr,
};

#[derive(Default)]
pub struct Rules {
    redirects: HashMap<String, HeaderValue>,
    ips: HashSet<IpAddr>,
    paths: HashSet<String>,
    all: bool,
}
impl Rules {
    pub fn load(dir: &Dir, enabled: bool) -> Result<Self> {
        if !enabled {
            return Ok(Self::default());
        }
        let file = match crate::assets::open_regular(dir, ".htaccess") {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).context("open .htaccess"),
        };
        let mut text = String::new();
        file.take(1024 * 1024 + 1).read_to_string(&mut text)?;
        ensure!(text.len() <= 1024 * 1024, ".htaccess exceeds 1 MiB");
        Self::parse(&text)
    }
    fn parse(text: &str) -> Result<Self> {
        let mut r = Self::default();
        for (i, line) in text.lines().enumerate() {
            let words: Vec<_> = line
                .split_whitespace()
                .take_while(|p| !p.starts_with('#'))
                .collect();
            match words.as_slice() {
                [] => (),
                ["deny", "from", "all"] => r.all = true,
                ["deny", "from", who] if who.starts_with('/') => {
                    r.paths.insert(
                        crate::paths::decode(who)
                            .map_err(|_| anyhow::anyhow!("invalid deny path at line {}", i + 1))?
                            .into_owned(),
                    );
                }
                ["deny", "from", who] => {
                    r.ips.insert(
                        who.parse().with_context(|| {
                            format!("invalid deny IP at .htaccess line {}", i + 1)
                        })?,
                    );
                }
                ["redirect", src, dst] => {
                    let path = crate::paths::decode(src).map_err(|_| {
                        anyhow::anyhow!("invalid redirect source at line {}", i + 1)
                    })?;
                    ensure!(
                        (dst.starts_with('/') && !dst.starts_with("//") && !dst.contains('\\'))
                            || reqwest::Url::parse(dst)
                                .is_ok_and(|u| matches!(u.scheme(), "http" | "https")
                                    && u.host().is_some()),
                        "bad redirect target at line {}",
                        i + 1
                    );
                    r.redirects
                        .insert(path.into_owned(), HeaderValue::from_str(dst)?);
                }
                _ => bail!("unknown .htaccess directive on line {}", i + 1),
            }
        }
        Ok(r)
    }
    pub fn denied(&self, ip: IpAddr, path: &str) -> bool {
        self.all || self.ips.contains(&ip) || self.paths.contains(path)
    }
    pub fn redirect(&self, path: &str) -> Option<&HeaderValue> {
        self.redirects.get(path)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rules() {
        let r = Rules::parse("deny from 127.0.0.1\nredirect /a /b\ndeny from /local\n").unwrap();
        assert!(r.denied("127.0.0.1".parse().unwrap(), "/"));
        assert!(r.denied("::1".parse().unwrap(), "/local"));
        assert_eq!(r.redirect("/a").unwrap(), "/b");
        assert!(Rules::parse("deny frm x").is_err());
    }
}
