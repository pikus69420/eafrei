use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub web: Web,
    #[serde(default)]
    pub php: Php,
    #[serde(default)]
    pub mail: Mail,
    #[serde(default)]
    pub security: Security,
    #[serde(default)]
    pub load_balancer: Proxy,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Web {
    pub bind: IpAddr,
    pub port: u16,
    pub tls_port: u16,
    pub static_dir: PathBuf,
    pub default_page: String,
    pub tls_cert: PathBuf,
    pub tls_key: PathBuf,
    pub enable_htaccess: bool,
    pub enable_load_balancer: bool,
    pub cache_bytes: usize,
    pub cache_file_bytes: usize,
}
impl Default for Web {
    fn default() -> Self {
        Self {
            bind: Ipv4Addr::LOCALHOST.into(),
            port: 8080,
            tls_port: 0,
            static_dir: "../public".into(),
            default_page: "index.html".into(),
            tls_cert: "../certs/cert.pem".into(),
            tls_key: "../certs/key.pem".into(),
            enable_htaccess: true,
            enable_load_balancer: false,
            cache_bytes: 16 << 20,
            cache_file_bytes: 64 << 10,
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Php {
    pub enabled: bool,
    pub endpoint: String,
    pub max_body_bytes: usize,
    pub max_concurrency: usize,
    pub timeout_secs: u64,
}
impl Default for Php {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "127.0.0.1:9000".into(),
            max_body_bytes: 8 << 20,
            max_concurrency: 16,
            timeout_secs: 30,
        }
    }
}
impl Php {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Proxy {
    pub upstreams: Vec<String>,
    pub timeout_secs: u64,
}
impl Default for Proxy {
    fn default() -> Self {
        Self {
            upstreams: vec![],
            timeout_secs: 30,
        }
    }
}
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Security {
    pub rate_limit: u32,
    pub reload_token: String,
}
impl Default for Security {
    fn default() -> Self {
        Self {
            rate_limit: 100,
            reload_token: String::new(),
        }
    }
}
#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Mail {
    pub enabled: bool,
    pub bind: IpAddr,
    pub port: u16,
    pub spool_dir: PathBuf,
}
impl Default for Mail {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: Ipv4Addr::LOCALHOST.into(),
            port: 2525,
            spool_dir: "../var/mail".into(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut c: Self = toml::from_str(&text).map_err(|e: toml::de::Error| {
            anyhow::anyhow!("invalid config {}: {}", path.display(), e.message())
        })?;
        let parent = path.parent().context("config has no parent directory")?;
        for p in [
            &mut c.web.static_dir,
            &mut c.web.tls_cert,
            &mut c.web.tls_key,
            &mut c.mail.spool_dir,
        ] {
            *p = parent.join(&*p);
        }
        c.web.static_dir = fs::canonicalize(&c.web.static_dir)
            .with_context(|| format!("document root {}", c.web.static_dir.display()))?;
        ensure!(c.web.static_dir.is_dir(), "static_dir must be a directory");
        ensure!(
            c.web.port > 0 && c.web.port != c.web.tls_port,
            "invalid or overlapping HTTP/TLS ports"
        );
        ensure!(
            crate::paths::public_segment(&c.web.default_page)
                && (crate::paths::mime(&c.web.default_page).is_some()
                    || crate::paths::is_php(&c.web.default_page)),
            "default_page must be one public filename"
        );
        ensure!(
            c.web.cache_file_bytes <= 64 << 20 && c.web.cache_bytes <= 1024 << 20,
            "cache limits too large"
        );
        ensure!(
            (1..=600).contains(&c.php.timeout_secs)
                && (1..=3600).contains(&c.load_balancer.timeout_secs),
            "invalid backend timeout"
        );
        ensure!(
            (1..=1024).contains(&c.php.max_concurrency) && c.php.max_body_bytes <= 128 << 20,
            "invalid PHP limits"
        );
        if let Some(sock) = c.php.endpoint.strip_prefix("unix:") {
            ensure!(
                cfg!(unix) && Path::new(sock).is_absolute(),
                "PHP needs an absolute Unix socket path on Unix"
            );
        } else {
            let addr: SocketAddr = c
                .php
                .endpoint
                .parse()
                .context("PHP endpoint must be a loopback IP:port or unix:/path")?;
            ensure!(
                addr.ip().is_loopback() && addr.port() > 0,
                "FastCGI TCP must be on loopback"
            );
        }
        for origin in &mut c.load_balancer.upstreams {
            let u = reqwest::Url::parse(origin).context("invalid upstream URL")?;
            ensure!(
                matches!(u.scheme(), "http" | "https")
                    && u.host().is_some()
                    && u.username().is_empty()
                    && u.password().is_none()
                    && u.path() == "/"
                    && u.query().is_none()
                    && u.fragment().is_none(),
                "upstreams must be HTTP(S) origins without credentials, path or query"
            );
            *origin = u.origin().ascii_serialization();
        }
        ensure!(
            !c.web.enable_load_balancer || !c.load_balancer.upstreams.is_empty(),
            "load balancing needs at least one upstream"
        );
        if c.mail.enabled && (!c.mail.bind.is_loopback() || c.mail.port == 0) {
            bail!("the unauthenticated SMTP inbox must use a loopback IP and a nonzero port");
        }
        Ok(c)
    }
    pub fn same_listeners(&self, new: &Self) -> bool {
        (
            self.web.bind,
            self.web.port,
            self.web.tls_port,
            &self.web.tls_cert,
            &self.web.tls_key,
            &self.mail,
        ) == (
            new.web.bind,
            new.web.port,
            new.web.tls_port,
            &new.web.tls_cert,
            &new.web.tls_key,
            &new.mail,
        )
    }
}
