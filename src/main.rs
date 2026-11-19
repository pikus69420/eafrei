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
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{sync::watch, task::JoinSet};
use web::{App, Link, Snapshot};

fn args() -> Result<Option<(PathBuf, bool)>> {
    let mut args = env::args_os().skip(1);
    let mut explicit = None;
    let mut check = false;
    while let Some(arg) = args.next() {
        if arg == "--config" {
            explicit = Some(PathBuf::from(
                args.next().context("--config requires a path")?,
            ));
        } else if arg == "--check" {
            check = true;
        } else if arg == "--version" {
            println!("eafrei {}", env!("CARGO_PKG_VERSION"));
            return Ok(None);
        } else if arg == "--help" || arg == "-h" {
            println!("eafrei [--config FILE] [--check]\n--check validates configuration, assets, rules and TLS without binding ports.\nConfig: --config > EAFREI_CONFIG > /etc/eafrei/eafrei.conf (Linux) > config/eafrei.conf.");
            return Ok(None);
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
            let p = Path::new("/etc/eafrei/eafrei.conf");
            if cfg!(target_os = "linux") && p.exists() {
                p.into()
            } else {
                "config/eafrei.conf".into()
            }
        });
    Ok(Some((std::path::absolute(path)?, check)))
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
#[tokio::main]
async fn main() -> Result<()> {
    let Some((path, check)) = args()? else {
        return Ok(());
    };
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
    if check {
        println!(
            "OK: {} ({} asset bytes preloaded)",
            path.display(),
            site.catalog.bytes
        );
        return Ok(());
    }
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
