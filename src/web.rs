use crate::{
    assets::{self, Asset, Catalog, Found},
    config::Config,
    htaccess::Rules,
    limit::Limiter,
    paths, php, proxy,
};
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header as h, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    Extension, Router,
};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{atomic::AtomicUsize, Arc},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, Semaphore};

#[derive(Clone, Copy)]
pub struct Link {
    pub addr: SocketAddr,
    pub tls: bool,
}
impl Link {
    pub fn scheme(self) -> &'static str {
        if self.tls {
            "https"
        } else {
            "http"
        }
    }
}
pub struct Snapshot {
    pub cfg: Config,
    pub catalog: Catalog,
    rules: Rules,
    pub php_slots: Arc<Semaphore>,
}
impl Snapshot {
    pub fn load(path: &Path) -> Result<Self> {
        let cfg = Config::load(path)?;
        let catalog = Catalog::build(&cfg).context("index document root")?;
        let rules = Rules::load(&catalog.root, cfg.web.enable_htaccess)?;
        let php_slots = Arc::new(Semaphore::new(cfg.php.max_concurrency));
        Ok(Self {
            cfg,
            catalog,
            rules,
            php_slots,
        })
    }
}
pub struct App {
    live: ArcSwap<Snapshot>,
    path: PathBuf,
    limiter: Limiter,
    rr: AtomicUsize,
    client: reqwest::Client,
    reload: Mutex<()>,
}
impl App {
    pub fn new(path: PathBuf, site: Snapshot) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            live: ArcSwap::from_pointee(site),
            path,
            limiter: Limiter::default(),
            rr: AtomicUsize::new(0),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .pool_max_idle_per_host(4)
                .pool_idle_timeout(Duration::from_secs(15))
                .connect_timeout(Duration::from_secs(5))
                .build()?,
            reload: Mutex::new(()),
        }))
    }
    pub fn router(self: &Arc<Self>, link: Link) -> Router {
        Router::new()
            .fallback(dispatch)
            .layer(Extension(link))
            .with_state(self.clone())
    }
    pub async fn reload(&self) -> std::result::Result<(), StatusCode> {
        let _guard = self.reload.try_lock().map_err(|_| StatusCode::CONFLICT)?;
        let path = self.path.clone();
        let next = tokio::task::spawn_blocking(move || Snapshot::load(&path))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .map_err(|e| {
                eprintln!("reload rejected: {e:#}");
                StatusCode::UNPROCESSABLE_ENTITY
            })?;
        if !self.live.load().cfg.same_listeners(&next.cfg) {
            eprintln!("reload rejected: listener/TLS/mail settings require a restart");
            return Err(StatusCode::CONFLICT);
        }
        self.live.store(Arc::new(next));
        Ok(())
    }
}
async fn dispatch(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Extension(link): Extension<Link>,
    req: Request,
) -> Response {
    let mut res = route(&app, peer, link, req).await;
    res.headers_mut().insert(
        h::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    res
}
async fn route(app: &App, peer: SocketAddr, link: Link, req: Request) -> Response {
    let site = app.live.load_full();
    if !app
        .limiter
        .allows(peer.ip(), site.cfg.security.rate_limit, Instant::now())
    {
        return (StatusCode::TOO_MANY_REQUESTS, [(h::RETRY_AFTER, "60")]).into_response();
    }
    let path = match paths::decode(req.uri().path()) {
        Ok(p) => p,
        Err(code) => return code.into_response(),
    };
    if site.rules.denied(peer.ip(), &path) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(location) = site.rules.redirect(&path) {
        return (
            StatusCode::MOVED_PERMANENTLY,
            [(h::LOCATION, location.clone())],
        )
            .into_response();
    }
    if path == "/admin/reload" {
        let token = &site.cfg.security.reload_token;
        if token.is_empty() {
            return StatusCode::NOT_FOUND.into_response();
        }
        if req.method() != Method::POST {
            return (StatusCode::METHOD_NOT_ALLOWED, [(h::ALLOW, "POST")]).into_response();
        }
        let auth = req
            .headers()
            .get(h::AUTHORIZATION)
            .and_then(|v| v.as_bytes().strip_prefix(b"Bearer "))
            .unwrap_or_default();
        if !bool::from(auth.ct_eq(token.as_bytes())) {
            return StatusCode::FORBIDDEN.into_response();
        }
        return match app.reload().await {
            Ok(()) => (StatusCode::OK, "config reloaded\n").into_response(),
            Err(s) => s.into_response(),
        };
    }
    match site.catalog.lookup(&path) {
        Found::File(name, entry) => {
            if name != path && site.rules.denied(peer.ip(), name) {
                return StatusCode::FORBIDDEN.into_response();
            }
            if matches!(entry, Asset::Php) {
                php::serve(&site, name, peer, link, req).await
            } else {
                assets::serve(&site.catalog.root, name, entry, req.method(), req.headers()).await
            }
        }
        Found::Slash => {
            let mut to = format!("{}/", req.uri().path());
            if let Some(q) = req.uri().query() {
                to.push('?');
                to.push_str(q);
            }
            (StatusCode::PERMANENT_REDIRECT, [(h::LOCATION, to)]).into_response()
        }
        Found::Missing if site.cfg.web.enable_load_balancer && !paths::reserved_script(&path) => {
            proxy::forward(
                &app.client,
                &app.rr,
                &site.cfg.load_balancer,
                peer,
                link,
                req,
            )
            .await
        }
        Found::Missing => StatusCode::NOT_FOUND.into_response(),
    }
}
