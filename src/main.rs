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

fn add_email_acc(name: &str) -> Result<()> {
    loop {
        println!("Password for {name}:");
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            bail!("unexpected end of input");
        }
        let pwd = line.trim_end_matches(&['\r', '\n'][..]);
        if pwd.chars().count() >= 4 {
            break;
        }
    }
    loop {
        println!("Set mail account {name} as no-reply?");
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            bail!("unexpected end of input");
        }
        let ans = line.trim();
        if ans.eq_ignore_ascii_case("y") || ans.eq_ignore_ascii_case("n") {
            break;
        }
    }
    Ok(())
}

enum Action {
    Serve(PathBuf),
    Check(PathBuf),
    Reload,
}

fn args() -> Result<Option<Action>> {
    let mut args = env::args_os().skip(1);
    let mut explicit = None;
    let mut check = false;
    let mut reload = false;
    while let Some(arg) = args.next() {
        if arg == "--config" {
            explicit = Some(PathBuf::from(
                args.next().context("--config requires a path")?,
            ));
        } else if arg == "--check" || arg == "check" {
            check = true;
        } else if arg == "reload" {
            reload = true;
        } else if arg == "--version" {
            println!("eafrei {}", env!("CARGO_PKG_VERSION"));
            return Ok(None);
        } else if arg == "--help" || arg == "-h" {
            println!("eafrei [--config DIR] [--check]\neafrei check\neafrei reload\nConfig: --config > EAFREI_CONFIG > /etc/eafrei > config");
            return Ok(None);
        } else if arg == "add" {
            let sub = args.next().context("missing subcommand for add")?;
            if sub == "email-acc" {
                let name = args.next().context("missing account name")?;
                let name = name.to_str().context("invalid account name")?;
                if name.is_empty() {
                    bail!("account name cannot be empty");
                }
                if let Some(extra) = args.next() {
                    bail!("unexpected argument: {extra:?}");
                }
                add_email_acc(name)?;
                return Ok(None);
            } else {
                bail!("unknown option: {sub:?}");
            }
        } else {
            bail!("unknown option: {arg:?}");
        }
    }
    if reload {
        return Ok(Some(Action::Reload));
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

#[tokio::main]
async fn main() -> Result<()> {
    let Some(action) = args()? else {
        return Ok(());
    };
    match action {
        Action::Check(path) => {
            let (count, bad_files) = config::Config::check(&path);
            if count == 0 {
                println!("Configuration is OK");
                return Ok(());
            } else {
                println!("Found {} error(s) at these files: [{}]", count, bad_files.join(", "));
                std::process::exit(1);
            }
        }
        Action::Reload => {
            #[cfg(unix)]
            {
                match config::find_server_pid() {
                    Some(pid) => {
                        config::send_hup(pid)?;
                        println!("Configuration reloaded");
                        return Ok(());
                    }
                    None => {
                        bail!("eafrei server is not running");
                    }
                }
            }
            #[cfg(not(unix))]
            {
                bail!("reload is not supported on this platform");
            }
        }
        Action::Serve(path) => {
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
            let _ = fs::write("/tmp/eafrei.pid", std::process::id().to_string());
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
    }
}
