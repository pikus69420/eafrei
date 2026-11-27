#![forbid(unsafe_code)]
mod assets;
mod config;
mod htaccess;
mod limit;
mod mail;
mod paths;
mod php;
mod proxy;
mod web;

use anyhow::{bail, Context, Result};
use axum_server::{accept::NoDelayAcceptor, tls_rustls::RustlsConfig, Handle};
use std::{
    env,
    fs,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{sync::watch, task::JoinSet};
use web::{App, Link, Snapshot};

fn add_email_acc(config_path: &Path, name: &str) -> Result<()> {
    let dir = config::Config::config_dir(config_path);
    let lang = config::get_lang(&dir);
    let is_pl = lang == "pl";
    loop {
        if is_pl {
            println!("Hasło dla {name}:");
        } else {
            println!("Password for {name}:");
        }
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            bail!("unexpected end of input");
        }
        let pwd = line.trim_end_matches(&['\r', '\n'][..]);
        if pwd.chars().count() >= 4 {
            let is_no_reply = loop {
                if is_pl {
                    println!("Ustawić konto pocztowe {name} jako tylko skrzynkę odbiorczą? (y/n)");
                } else {
                    println!("Set mail account {name} as no-reply? (y/n)");
                }
                let mut ans_line = String::new();
                if std::io::stdin().read_line(&mut ans_line)? == 0 {
                    bail!("unexpected end of input");
                }
                let ans = ans_line.trim();
                if ans.eq_ignore_ascii_case("y") || ans.eq_ignore_ascii_case("tak") || ans.eq_ignore_ascii_case("t") {
                    break true;
                } else if ans.eq_ignore_ascii_case("n") || ans.eq_ignore_ascii_case("nie") {
                    break false;
                }
            };
            let spool_dir = if let Ok(cfg) = config::Config::load(config_path) {
                cfg.mail.spool_dir
            } else {
                dir.join("../var/mail")
            };
            mail::save_account(&spool_dir, name, pwd, is_no_reply)?;
            if is_pl {
                println!("Konto pocztowe {name} zostało pomyślnie zapisane.");
            } else {
                println!("Email account {name} saved successfully.");
            }
            break;
        }
    }
    Ok(())
}

fn read_rss_kb(pid: u32) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            return Some(kb);
        }
    }
    None
}

fn print_status(path: &Path) {
    let dir = config::Config::config_dir(path);
    let is_pl = config::get_lang(&dir) == "pl";
    match config::find_server_pid() {
        Some(pid) => {
            if is_pl {
                println!("eafrei: działa (PID {pid})");
            } else {
                println!("eafrei: running (PID {pid})");
            }
            if let Some(kb) = read_rss_kb(pid) {
                println!("RAM: {:.1} MB", kb as f64 / 1024.0);
            }
            if let Ok(cfg) = config::Config::load(path) {
                println!("HTTP: http://{}:{}", cfg.web.bind, cfg.web.port);
                if cfg.web.tls_port != 0 {
                    println!("HTTPS: https://{}:{}", cfg.web.bind, cfg.web.tls_port);
                }
                if cfg.mail.enabled {
                    println!("SMTP: {}:{}", cfg.mail.bind, cfg.mail.port);
                }
            }
        }
        None => {
            if is_pl {
                println!("eafrei: zatrzymany");
            } else {
                println!("eafrei: stopped");
            }
        }
    }
}

enum Action {
    Serve(PathBuf),
    Check(PathBuf),
    Reload(PathBuf),
    Status(PathBuf),
    SetLang(PathBuf, String),
    AddEmail(PathBuf, String),
}

fn args() -> Result<Option<Action>> {
    let mut args = env::args_os().skip(1);
    let mut explicit = None;
    let mut check = false;
    let mut reload = false;
    let mut status = false;
    let mut set_lang = None;
    let mut add_email = None;
    while let Some(arg) = args.next() {
        if arg == "config" {
            explicit = Some(PathBuf::from(
                args.next().context("config requires a path")?,
            ));
        } else if arg == "check" {
            check = true;
        } else if arg == "reload" {
            reload = true;
        } else if arg == "status" {
            status = true;
        } else if arg == "set-lang" {
            let l = args.next().context("missing language code (pl or en)")?;
            let ls = l.to_str().context("invalid language code")?;
            if ls != "pl" && ls != "en" {
                bail!("unsupported language: {ls}. Supported: pl, en");
            }
            set_lang = Some(ls.to_string());
        } else if arg == "version" {
            println!("eafrei {}", env!("CARGO_PKG_VERSION"));
            return Ok(None);
        } else if arg == "help" || arg == "-h" {
            let default_dir = Path::new("config");
            let lang = config::get_lang(default_dir);
            if lang == "pl" {
                println!("eafrei [config KATALOG]\neafrei check\neafrei reload\neafrei status\neafrei set-lang [pl|en]\neafrei add email-acc <nazwa>\neafrei help\neafrei version\nKonfiguracja: config > EAFREI_CONFIG > /etc/eafrei > config");
            } else {
                println!("eafrei [config DIR]\neafrei check\neafrei reload\neafrei status\neafrei set-lang [pl|en]\neafrei add email-acc <name>\neafrei help\neafrei version\nConfig: config > EAFREI_CONFIG > /etc/eafrei > config");
            }
            return Ok(None);
        } else if arg == "add" {
            let sub = args.next().context("missing subcommand for add")?;
            if sub == "email-acc" {
                let name = args.next().context("missing account name")?;
                let name = name.to_str().context("invalid account name")?;
                if name.is_empty() {
                    bail!("account name cannot be empty");
                }
                add_email = Some(name.to_string());
            } else {
                bail!("unknown option: {sub:?}");
            }
        } else {
            bail!("unknown option: {arg:?}");
        }
    }
    let path = explicit
        .or_else(|| {
            env::var_os("EAFREI_CONFIG")
                .filter(|p| !p.is_empty())
                .map(Into::into)
        })
        .unwrap_or_else(|| {
            let p = Path::new("/etc/eafrei");
            if cfg!(target_os = "linux") && p.exists() {
                p.into()
            } else {
                "config".into()
            }
        });
    let abs_path = std::path::absolute(path)?;
    if let Some(l) = set_lang {
        return Ok(Some(Action::SetLang(abs_path, l)));
    }
    if let Some(name) = add_email {
        return Ok(Some(Action::AddEmail(abs_path, name)));
    }
    if reload {
        return Ok(Some(Action::Reload(abs_path)));
    }
    if status {
        return Ok(Some(Action::Status(abs_path)));
    }
    if check {
        Ok(Some(Action::Check(abs_path)))
    } else {
        Ok(Some(Action::Serve(abs_path)))
    }
}

#[cfg(unix)]
fn on_hup(app: std::sync::Arc<App>) -> Result<()> {
    let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    tokio::spawn(async move {
        while sig.recv().await.is_some() {
            match app.reload().await {
                Ok(()) => eprintln!("reloaded"),
                Err(code) => eprintln!("reload failed: {code}"),
            }
        }
    });
    Ok(())
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {r=tokio::signal::ctrl_c()=>r?,_=term.recv()=>()}
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

struct PidFileGuard;
impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file("/tmp/eafrei.pid");
    }
}

fn main() -> Result<()> {
    let Some(action) = args()? else {
        return Ok(());
    };
    match action {
        Action::SetLang(path, lang) => {
            let dir = config::Config::config_dir(&path);
            config::save_lang(&dir, &lang)?;
            if lang == "pl" {
                println!("Język został przełączony na polski (pl)");
            } else {
                println!("Language switched to English (en)");
            }
            return Ok(());
        }
        Action::AddEmail(path, name) => {
            add_email_acc(&path, &name)?;
            return Ok(());
        }
        Action::Check(path) => {
            let (count, bad_files) = config::Config::check(&path);
            if count == 0 {
                println!("Configuration is OK");
                return Ok(());
            } else {
                println!("Found {} error(s) at these files: {}", count, bad_files.join(", "));
                std::process::exit(1);
            }
        }
        Action::Reload(path) => {
            let dir = config::Config::config_dir(&path);
            let is_pl = config::get_lang(&dir) == "pl";
            #[cfg(unix)]
            {
                match config::find_server_pid() {
                    Some(pid) => {
                        config::send_hup(pid)?;
                        if is_pl {
                            println!("Konfiguracja przeładowana");
                        } else {
                            println!("Configuration reloaded");
                        }
                        return Ok(());
                    }
                    None => {
                        if is_pl {
                            bail!("serwer eafrei nie jest uruchomiony");
                        } else {
                            bail!("eafrei server is not running");
                        }
                    }
                }
            }
            #[cfg(not(unix))]
            {
                bail!("reload is not supported on this platform");
            }
        }
        Action::Status(path) => {
            print_status(&path);
            return Ok(());
        }
        Action::Serve(path) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .max_blocking_threads(4)
                .build()?;
            rt.block_on(serve(path))
        }
    }
}

async fn serve(path: PathBuf) -> Result<()> {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let site = Snapshot::load(&path).with_context(|| format!("load {}", path.display()))?;
            let cfg = site.cfg.clone();
            let web = &cfg.web;
            let tls = if web.tls_port != 0 {
                Some(
                    RustlsConfig::from_pem_file(&web.tls_cert, &web.tls_key)
                        .await
                        .context("TLS certificate/key (use tls_port = 0 to disable)")?,
                )
            } else {
                None
            };
            let app = App::new(path, site)?;
            let http = SocketAddr::new(web.bind, web.port);
            let plain = TcpListener::bind(http).with_context(|| format!("bind HTTP {http}"))?;
            let secure = if let Some(tls) = tls {
                let addr = SocketAddr::new(web.bind, web.tls_port);
                Some((
                    TcpListener::bind(addr).with_context(|| format!("bind HTTPS {addr}"))?,
                    addr,
                    tls,
                ))
            } else {
                None
            };
            let smtp = if cfg.mail.enabled {
                tokio::fs::create_dir_all(&cfg.mail.spool_dir)
                    .await
                    .context("create mail spool")?;
                let addr = SocketAddr::new(cfg.mail.bind, cfg.mail.port);
                Some(
                    tokio::net::TcpListener::bind(addr)
                        .await
                        .with_context(|| format!("bind SMTP {addr}"))?,
                )
            } else {
                None
            };
            #[cfg(unix)]
            on_hup(app.clone())?;
            if let Ok(meta) = fs::symlink_metadata("/tmp/eafrei.pid") {
                if meta.file_type().is_symlink() {
                    let _ = fs::remove_file("/tmp/eafrei.pid");
                }
            }
            let _ = fs::write("/tmp/eafrei.pid", std::process::id().to_string());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions("/tmp/eafrei.pid", fs::Permissions::from_mode(0o600));
            }
            let _pid_guard = PidFileGuard;
            let mut jobs = JoinSet::<Result<()>>::new();
            let mut handles = Vec::new();
            let (stop, rx) = watch::channel(false);
            if let Some(l) = smtp {
                let dir = cfg.mail.spool_dir;
                println!("SMTP {} (local inbox)", l.local_addr()?);
                jobs.spawn(async move { mail::serve(l, &dir, rx).await });
            }
            if let Some((l, a, tls)) = secure {
                let h = Handle::new();
                handles.push(h.clone());
                let r = app.router(Link { addr: a, tls: true });
                println!("HTTPS https://{a}");
                jobs.spawn(async move {
                    axum_server::from_tcp_rustls(l, tls)
                        .map(|tls| tls.acceptor(NoDelayAcceptor::new()))
                        .handle(h)
                        .serve(r.into_make_service_with_connect_info::<SocketAddr>())
                        .await
                        .context("HTTPS server")
                });
            }
            let h = Handle::new();
            handles.push(h.clone());
            let r = app.router(Link {
                addr: http,
                tls: false,
            });
            println!("HTTP http://{http}");
            jobs.spawn(async move {
                axum_server::from_tcp(plain)
                    .acceptor(NoDelayAcceptor::new())
                    .handle(h)
                    .serve(r.into_make_service_with_connect_info::<SocketAddr>())
                    .await
                    .context("HTTP server")
            });
            tokio::select! {s=shutdown_signal()=>s?,done=jobs.join_next()=>{done.context("no servers")???;bail!("listener stopped unexpectedly");}}
            for h in handles {
                h.graceful_shutdown(Some(Duration::from_secs(5)));
            }
            let _ = stop.send(true);
            while let Some(job) = jobs.join_next().await {
                job??;
            }
            Ok(())
}
