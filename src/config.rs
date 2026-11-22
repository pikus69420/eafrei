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
            cache_bytes: 8 << 20,
            cache_file_bytes: 32 << 10,
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
            max_body_bytes: 4 << 20,
            max_concurrency: 4,
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

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct MailFile {
    pub mail: Mail,
}

fn parse_mail(text: &str) -> std::result::Result<Mail, String> {
    if let Ok(file) = toml::from_str::<MailFile>(text) {
        return Ok(file.mail);
    }
    match toml::from_str::<Mail>(text) {
        Ok(m) => Ok(m),
        Err(e) => Err(e.to_string()),
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PhpFile {
    pub php: Php,
}

fn parse_php(text: &str) -> std::result::Result<Php, String> {
    if let Ok(file) = toml::from_str::<PhpFile>(text) {
        return Ok(file.php);
    }
    match toml::from_str::<Php>(text) {
        Ok(p) => Ok(p),
        Err(e) => Err(e.to_string()),
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebFile {
    pub web: Web,
    #[serde(default)]
    pub load_balancer: Proxy,
    #[serde(default)]
    pub security: Security,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlatWebFile {
    #[serde(flatten)]
    pub web: Web,
    #[serde(default)]
    pub load_balancer: Proxy,
    #[serde(default)]
    pub security: Security,
}

fn parse_web(text: &str) -> std::result::Result<(Web, Proxy, Security), String> {
    if let Ok(file) = toml::from_str::<WebFile>(text) {
        return Ok((file.web, file.load_balancer, file.security));
    }
    match toml::from_str::<FlatWebFile>(text) {
        Ok(file) => Ok((file.web, file.load_balancer, file.security)),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map_or(false, |s| s.success())
}
#[cfg(not(unix))]
pub fn is_process_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
pub fn send_hup(pid: u32) -> Result<()> {
    let status = std::process::Command::new("kill")
        .args(["-HUP", &pid.to_string()])
        .status()
        .context("run kill command")?;
    if status.success() {
        Ok(())
    } else {
        bail!("failed to signal PID {pid}")
    }
}

pub fn find_server_pid() -> Option<u32> {
    if let Ok(content) = fs::read_to_string("/tmp/eafrei.pid") {
        if let Ok(pid) = content.trim().parse::<u32>() {
            if pid != std::process::id() && is_process_alive(pid) {
                return Some(pid);
            }
        }
    }
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            if let Ok(name) = entry.file_name().into_string() {
                if let Ok(pid) = name.parse::<u32>() {
                    if pid != std::process::id() && is_process_alive(pid) {
                        let comm_path = format!("/proc/{pid}/comm");
                        if let Ok(comm) = fs::read_to_string(&comm_path) {
                            if comm.trim() == "eafrei" {
                                return Some(pid);
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

pub fn save_lang(dir: &Path, lang: &str) -> Result<()> {
    let _ = fs::write(dir.join(".lang"), lang);
    let _ = fs::write("config/.lang", lang);
    Ok(())
}

pub fn get_lang(dir: &Path) -> String {
    if let Ok(content) = fs::read_to_string(dir.join(".lang")) {
        let trimmed = content.trim().to_lowercase();
        if trimmed == "pl" || trimmed == "en" {
            return trimmed;
        }
    }
    if let Ok(content) = fs::read_to_string("config/.lang") {
        let trimmed = content.trim().to_lowercase();
        if trimmed == "pl" || trimmed == "en" {
            return trimmed;
        }
    }
    "en".to_string()
}

impl Config {
    pub fn config_dir(path: &Path) -> PathBuf {
        if path.is_dir() {
            path.to_path_buf()
        } else if let Some(parent) = path.parent() {
            if parent.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                parent.to_path_buf()
            }
        } else {
            PathBuf::from(".")
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let dir = Self::config_dir(path);
        let mail_path = dir.join("mail.conf");
        let php_path = dir.join("php.conf");
        let web_path = dir.join("web.conf");

        let mail_text = fs::read_to_string(&mail_path)
            .with_context(|| format!("read {}", mail_path.display()))?;
        let mail = parse_mail(&mail_text)
            .map_err(|e| anyhow::anyhow!("invalid config {}: {}", mail_path.display(), e))?;

        let php_text = fs::read_to_string(&php_path)
            .with_context(|| format!("read {}", php_path.display()))?;
        let php = parse_php(&php_text)
            .map_err(|e| anyhow::anyhow!("invalid config {}: {}", php_path.display(), e))?;

        let web_text = fs::read_to_string(&web_path)
            .with_context(|| format!("read {}", web_path.display()))?;
        let (web, load_balancer, security) = parse_web(&web_text)
            .map_err(|e| anyhow::anyhow!("invalid config {}: {}", web_path.display(), e))?;

        let mut c = Self {
            web,
            php,
            mail,
            security,
            load_balancer,
        };

        for p in [
            &mut c.web.static_dir,
            &mut c.web.tls_cert,
            &mut c.web.tls_key,
            &mut c.mail.spool_dir,
        ] {
            *p = dir.join(&*p);
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
            c.web.cache_file_bytes <= 16 << 20 && c.web.cache_bytes <= 64 << 20,
            "cache limits too large"
        );
        ensure!(
            (1..=600).contains(&c.php.timeout_secs)
                && (1..=3600).contains(&c.load_balancer.timeout_secs),
            "invalid backend timeout"
        );
        ensure!(
            (1..=32).contains(&c.php.max_concurrency) && c.php.max_body_bytes <= 32 << 20,
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

    pub fn check(path: &Path) -> (usize, Vec<String>) {
        let dir = Self::config_dir(path);
        let mail_path = dir.join("mail.conf");
        let php_path = dir.join("php.conf");
        let web_path = dir.join("web.conf");

        let mut mail_errors = Vec::new();
        let mut php_errors = Vec::new();
        let mut web_errors = Vec::new();

        match fs::read_to_string(&mail_path) {
            Err(e) => mail_errors.push(e.to_string()),
            Ok(text) => match parse_mail(&text) {
                Err(e) => mail_errors.push(e),
                Ok(mail) => {
                    if mail.enabled {
                        if !mail.bind.is_loopback() {
                            mail_errors.push("the unauthenticated SMTP inbox must use a loopback IP".into());
                        }
                        if mail.port == 0 {
                            mail_errors.push("the unauthenticated SMTP inbox must use a nonzero port".into());
                        }
                    }
                }
            },
        }

        match fs::read_to_string(&php_path) {
            Err(e) => php_errors.push(e.to_string()),
            Ok(text) => match parse_php(&text) {
                Err(e) => php_errors.push(e),
                Ok(php) => {
                    if !(1..=600).contains(&php.timeout_secs) {
                        php_errors.push("invalid backend timeout".into());
                    }
                    if !(1..=32).contains(&php.max_concurrency) || php.max_body_bytes > 32 << 20 {
                        php_errors.push("invalid PHP limits".into());
                    }
                    if let Some(sock) = php.endpoint.strip_prefix("unix:") {
                        if !(cfg!(unix) && Path::new(sock).is_absolute()) {
                            php_errors.push("PHP needs an absolute Unix socket path on Unix".into());
                        }
                    } else {
                        match php.endpoint.parse::<SocketAddr>() {
                            Ok(addr) => {
                                if !addr.ip().is_loopback() || addr.port() == 0 {
                                    php_errors.push("FastCGI TCP must be on loopback".into());
                                }
                            }
                            Err(_) => {
                                php_errors.push("PHP endpoint must be a loopback IP:port or unix:/path".into());
                            }
                        }
                    }
                }
            },
        }

        match fs::read_to_string(&web_path) {
            Err(e) => web_errors.push(e.to_string()),
            Ok(text) => match parse_web(&text) {
                Err(e) => web_errors.push(e),
                Ok((web, load_balancer, _security)) => {
                    let static_dir = dir.join(&web.static_dir);
                    match fs::canonicalize(&static_dir) {
                        Ok(canon) => {
                            if !canon.is_dir() {
                                web_errors.push("static_dir must be a directory".into());
                            }
                        }
                        Err(_) => {
                            web_errors.push("static_dir does not exist".into());
                        }
                    }
                    if web.port == 0 || (web.tls_port != 0 && web.port == web.tls_port) {
                        web_errors.push("invalid or overlapping HTTP/TLS ports".into());
                    }
                    if !(crate::paths::public_segment(&web.default_page)
                        && (crate::paths::mime(&web.default_page).is_some()
                            || crate::paths::is_php(&web.default_page))) {
                        web_errors.push("default_page must be one public filename".into());
                    }
                    if web.cache_file_bytes > 16 << 20 || web.cache_bytes > 64 << 20 {
                        web_errors.push("cache limits too large".into());
                    }
                    if !(1..=3600).contains(&load_balancer.timeout_secs) {
                        web_errors.push("invalid backend timeout".into());
                    }
                    for origin in &load_balancer.upstreams {
                        match reqwest::Url::parse(origin) {
                            Ok(u) => {
                                if !(matches!(u.scheme(), "http" | "https")
                                    && u.host().is_some()
                                    && u.username().is_empty()
                                    && u.password().is_none()
                                    && u.path() == "/"
                                    && u.query().is_none()
                                    && u.fragment().is_none()) {
                                    web_errors.push("upstreams must be HTTP(S) origins without credentials, path or query".into());
                                }
                            }
                            Err(_) => {
                                web_errors.push("invalid upstream URL".into());
                            }
                        }
                    }
                    if web.enable_load_balancer && load_balancer.upstreams.is_empty() {
                        web_errors.push("load balancing needs at least one upstream".into());
                    }
                    if web.tls_port != 0 {
                        let cert = dir.join(&web.tls_cert);
                        let key = dir.join(&web.tls_key);
                        if !cert.exists() {
                            web_errors.push("tls_cert file does not exist".into());
                        }
                        if !key.exists() {
                            web_errors.push("tls_key file does not exist".into());
                        }
                    }
                }
            },
        }

        let total = mail_errors.len() + php_errors.len() + web_errors.len();
        let mut bad_files = Vec::new();
        if !mail_errors.is_empty() {
            bad_files.push("mail.conf".into());
        }
        if !php_errors.is_empty() {
            bad_files.push("php.conf".into());
        }
        if !web_errors.is_empty() {
            bad_files.push("web.conf".into());
        }
        (total, bad_files)
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
