mod qbit;

use std::{net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Form, Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use tower_http::compression::CompressionLayer;

use qbit::{Action, AddRequest, Qbit};

const COOKIE: &str = "qbm";
const MAX_UPLOAD: usize = 32 * 1024 * 1024;

struct App {
    qb: Qbit,
    /// Expected cookie value when UI_PASSWORD is set.
    session: Option<String>,
    password: Option<String>,
    cookie_path: String,
    /// Serializes password checks so the failure delay caps guesses globally,
    /// not per connection.
    login_gate: Mutex<()>,
}

type Shared = Arc<App>;

impl App {
    fn new(qb: Qbit, password: Option<String>, base: &str) -> Shared {
        Arc::new(App {
            qb,
            session: password.as_deref().map(session_token),
            password,
            cookie_path: format!("{base}/"),
            login_gate: Mutex::new(()),
        })
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "qbm=info".into()),
        )
        .init();

    let qb_url = env("QB_URL").unwrap_or_else(|| "http://127.0.0.1:8080".into());
    let listen: SocketAddr = env("LISTEN")
        .unwrap_or_else(|| "0.0.0.0:8099".into())
        .parse()
        .expect("LISTEN must be host:port");
    let base = env("BASE_PATH")
        .map(|b| format!("/{}", b.trim_matches('/')))
        .filter(|b| b != "/")
        .unwrap_or_default();
    let password = env("UI_PASSWORD");

    let app = App::new(
        Qbit::new(
            qb_url.clone(),
            env("QB_USER").unwrap_or_default(),
            env("QB_PASS").unwrap_or_default(),
        ),
        password,
        &base,
    );

    if app.password.is_none() {
        tracing::warn!(
            "UI_PASSWORD not set: anyone who can reach {listen} can control qBittorrent"
        );
    }

    let router = build_router(app, &base);

    tracing::info!("qbm listening on http://{listen}{base}/ -> {qb_url}");
    let listener = tokio::net::TcpListener::bind(listen).await.expect("bind");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server");
}

fn build_router(app: Shared, base: &str) -> Router {
    // every route carries the prefix itself; nest() would serve "/m" but 404 on "/m/"
    let at = |path: &str| format!("{base}{path}");

    let protected = Router::new()
        .route(&at("/"), get(index))
        .route(&at("/api/state"), get(api_state))
        .route(&at("/api/magnet/{hash}"), get(api_magnet))
        .route(&at("/api/add"), post(api_add))
        .route(&at("/api/action"), post(api_action))
        .route(&at("/api/altspeed"), post(api_altspeed))
        .route_layer(middleware::from_fn_with_state(app.clone(), require_auth));

    let mut public = Router::new()
        .route(&at("/login"), get(login_page).post(login_submit))
        .route(
            &at("/app.js"),
            get(|| asset("text/javascript", include_bytes!("../web/app.js"))),
        )
        .route(
            &at("/app.css"),
            get(|| asset("text/css", include_bytes!("../web/app.css"))),
        )
        .route(
            &at("/sw.js"),
            get(|| asset("text/javascript", include_bytes!("../web/sw.js"))),
        )
        .route(
            &at("/manifest.webmanifest"),
            get(|| {
                asset(
                    "application/manifest+json",
                    include_bytes!("../web/manifest.webmanifest"),
                )
            }),
        )
        .route(&at("/icons/{name}"), get(icon));

    if !base.is_empty() {
        // the page's relative URLs only resolve correctly with the trailing slash
        public = public.route(base, get(add_trailing_slash));
    }

    protected
        .merge(public)
        .layer(DefaultBodyLimit::max(MAX_UPLOAD))
        .layer(middleware::from_fn(same_origin_writes))
        // the 2 s state poll is repetitive JSON; gzip cuts it ~10x on mobile data
        .layer(CompressionLayer::new())
        .layer(middleware::from_fn(security_headers))
        .with_state(app)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // systemd stops services with SIGTERM, not SIGINT
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.ok();
}

// ---------------------------------------------------------------- static

async fn index() -> Response {
    let mut r = Html(include_str!("../web/index.html")).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

async fn add_trailing_slash(uri: Uri) -> Redirect {
    match uri.query() {
        Some(q) => Redirect::permanent(&format!("{}/?{q}", uri.path())),
        None => Redirect::permanent(&format!("{}/", uri.path())),
    }
}

async fn asset(mime: &'static str, body: &'static [u8]) -> Response {
    (
        [
            (header::CONTENT_TYPE, mime),
            // the service worker revalidates; this just avoids a stale shell
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn icon(Path(name): Path<String>) -> Response {
    let (mime, body): (&str, &'static [u8]) = match name.as_str() {
        "icon.svg" => ("image/svg+xml", include_bytes!("../web/icons/icon.svg")),
        "icon-192.png" => ("image/png", include_bytes!("../web/icons/icon-192.png")),
        "icon-512.png" => ("image/png", include_bytes!("../web/icons/icon-512.png")),
        "maskable-192.png" => ("image/png", include_bytes!("../web/icons/maskable-192.png")),
        "maskable-512.png" => ("image/png", include_bytes!("../web/icons/maskable-512.png")),
        "monochrome-512.png" => (
            "image/png",
            include_bytes!("../web/icons/monochrome-512.png"),
        ),
        "apple-touch-icon.png" => (
            "image/png",
            include_bytes!("../web/icons/apple-touch-icon.png"),
        ),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "public, max-age=604800"),
        ],
        body,
    )
        .into_response()
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' data:; style-src 'self'; \
             script-src 'self'; connect-src 'self'; manifest-src 'self'; \
             frame-ancestors 'none'; base-uri 'self'; form-action 'self'",
        ),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    r
}

/// Rejects state-changing requests started by another site. A plain HTML
/// form can POST multipart cross-origin without a CORS preflight, which would
/// otherwise let any web page add torrents when UI_PASSWORD is unset.
async fn same_origin_writes(req: Request, next: Next) -> Response {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        let site = req
            .headers()
            .get("sec-fetch-site")
            .and_then(|v| v.to_str().ok());
        // absent on curl and very old browsers, which aren't a CSRF vector
        if matches!(site, Some(s) if s != "same-origin" && s != "none") {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "cross-site request refused" })),
            )
                .into_response();
        }
    }
    next.run(req).await
}

// ------------------------------------------------------------------ auth

fn session_token(password: &str) -> String {
    let digest = Sha256::digest(format!("qbm-session-v1\0{password}"));
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn has_session(app: &App, headers: &HeaderMap) -> bool {
    let Some(expected) = &app.session else {
        return true;
    };

    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .any(|(k, v)| k == COOKIE && bool::from(v.as_bytes().ct_eq(expected.as_bytes())))
}

async fn require_auth(State(app): State<Shared>, req: Request, next: Next) -> Response {
    if has_session(&app, req.headers()) {
        return next.run(req).await;
    }

    if req.uri().path().contains("/api/") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "login required" })),
        )
            .into_response();
    }

    // keep ?add=magnet:... so a link opened while logged out still lands
    match req.uri().query() {
        Some(q) => Redirect::to(&format!("login?{q}")).into_response(),
        None => Redirect::to("login").into_response(),
    }
}

async fn login_page(State(app): State<Shared>, req: Request) -> Response {
    if app.password.is_none() {
        return Redirect::to("./").into_response();
    }
    render_login(req.uri().query().unwrap_or(""), false)
}

fn render_login(query: &str, failed: bool) -> Response {
    let q = html_escape(query);
    let error = if failed {
        r#"<p class="login-error">Wrong password</p>"#
    } else {
        ""
    };
    let page = format!(
        r##"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="theme-color" content="#f3f4f7" media="(prefers-color-scheme: light)">
<meta name="theme-color" content="#101114" media="(prefers-color-scheme: dark)">
<link rel="manifest" href="manifest.webmanifest">
<link rel="icon" href="icons/icon.svg" type="image/svg+xml">
<link rel="stylesheet" href="app.css">
<title>qb</title>
</head><body class="login">
<form method="post" action="login" class="login-card">
  <img src="icons/icon.svg" alt="qb" width="96" height="96">
  {error}
  <input type="password" name="password" placeholder="Password" autocomplete="current-password" autofocus required>
  <input type="hidden" name="q" value="{q}">
  <button type="submit" class="primary">Unlock</button>
</form>
</body></html>"##
    );
    let status = if failed {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::OK
    };
    (status, Html(page)).into_response()
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
    #[serde(default)]
    q: String,
}

/// True when a TLS-terminating proxy reports the browser connected over https.
fn behind_https(headers: &HeaderMap) -> bool {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    };
    header("x-forwarded-proto").eq_ignore_ascii_case("https")
        || header("forwarded")
            .to_ascii_lowercase()
            .contains("proto=https")
}

async fn login_submit(
    State(app): State<Shared>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let (Some(expected), Some(token)) = (&app.password, &app.session) else {
        return Redirect::to("./").into_response();
    };

    {
        let _gate = app.login_gate.lock().await;
        if !bool::from(form.password.as_bytes().ct_eq(expected.as_bytes())) {
            // held while sleeping: at most one guess per second across all clients
            tokio::time::sleep(Duration::from_secs(1)).await;
            return render_login(&form.q, true);
        }
    }

    // Secure only when we know the browser is on https: on a plain-http LAN
    // address the browser would drop the cookie and the login would never stick
    let secure = if behind_https(&headers) {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{COOKIE}={token}; Path={}; Max-Age=31536000; HttpOnly; SameSite=Lax{secure}",
        app.cookie_path
    );
    // "./?" keeps the redirect on this origin; a browser-supplied query is
    // already percent-encoded ASCII, and anything else would make
    // Redirect::to panic on an invalid header value
    let q: String = form.q.chars().filter(char::is_ascii_graphic).collect();
    let target = if q.is_empty() {
        "./".to_owned()
    } else {
        format!("./?{q}")
    };

    let mut r = Redirect::to(&target).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        r.headers_mut().insert(header::SET_COOKIE, v);
    }
    r
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ------------------------------------------------------------------- api

fn qb_error(e: qbit::Error) -> Response {
    tracing::warn!("{e}");
    let status = match e {
        qbit::Error::Rejected(_) => StatusCode::CONFLICT,
        _ => StatusCode::BAD_GATEWAY,
    };
    (status, Json(json!({ "error": e.to_string() }))).into_response()
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
}

fn ok() -> Response {
    Json(json!({ "ok": true })).into_response()
}

async fn api_state(State(app): State<Shared>) -> Response {
    match app.qb.snapshot().await {
        Ok(v) => {
            let mut r = Json(v).into_response();
            r.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            r
        }
        Err(e) => qb_error(e),
    }
}

async fn api_magnet(State(app): State<Shared>, Path(hash): Path<String>) -> Response {
    match app.qb.magnet(&hash).await {
        Some(m) => Json(json!({ "magnet": m })).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown torrent" })),
        )
            .into_response(),
    }
}

fn is_hash(h: &str) -> bool {
    matches!(h.len(), 40 | 64) && h.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Accepts magnet / http(s) / bc links and bare info-hashes.
fn normalize_link(line: &str) -> Option<String> {
    let l = line.trim();
    let lower = l.to_ascii_lowercase();
    if lower.starts_with("magnet:?")
        || lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("bc://bt/")
    {
        Some(l.to_owned())
    } else if is_hash(l) {
        Some(format!("magnet:?xt=urn:btih:{l}"))
    } else {
        None
    }
}

async fn api_add(State(app): State<Shared>, mut mp: Multipart) -> Response {
    let mut req = AddRequest {
        urls: vec![],
        files: vec![],
        category: String::new(),
        paused: false,
    };

    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return bad_request(&format!("bad upload: {e}")),
        };
        let name = field.name().unwrap_or_default().to_owned();
        if name == "torrents" {
            let file = field.file_name().unwrap_or("upload.torrent").to_owned();
            match field.bytes().await {
                // every .torrent is a bencoded dict
                Ok(b) if b.first() == Some(&b'd') => req.files.push((file, b.to_vec())),
                Ok(_) => return bad_request(&format!("{file} is not a .torrent file")),
                Err(e) => return bad_request(&format!("upload failed: {e}")),
            }
            continue;
        }

        let text = match field.text().await {
            Ok(t) => t,
            Err(e) => return bad_request(&format!("upload failed: {e}")),
        };
        match name.as_str() {
            "urls" => {
                for line in text.lines().filter(|l| !l.trim().is_empty()) {
                    match normalize_link(line) {
                        Some(l) => req.urls.push(l),
                        None => return bad_request("not a magnet link, torrent URL or info-hash"),
                    }
                }
            }
            "category" => req.category = text.trim().to_owned(),
            "paused" => req.paused = text == "true",
            _ => {}
        }
    }

    if req.urls.is_empty() && req.files.is_empty() {
        return bad_request("nothing to add");
    }

    let count = req.urls.len() + req.files.len();
    match app.qb.add(req).await {
        Ok(()) => Json(json!({ "ok": true, "count": count })).into_response(),
        Err(e) => qb_error(e),
    }
}

#[derive(Deserialize)]
struct ActionRequest {
    action: Action,
    hashes: Vec<String>,
}

async fn api_action(State(app): State<Shared>, Json(req): Json<ActionRequest>) -> Response {
    // never let "all" or other qBittorrent wildcards through
    if req.hashes.is_empty() || req.hashes.len() > 500 || !req.hashes.iter().all(|h| is_hash(h)) {
        return bad_request("invalid torrent hashes");
    }
    match app.qb.action(req.action, &req.hashes.join("|")).await {
        Ok(()) => ok(),
        Err(e) => qb_error(e),
    }
}

async fn api_altspeed(State(app): State<Shared>) -> Response {
    match app.qb.toggle_alt_speed().await {
        Ok(()) => ok(),
        Err(e) => qb_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use std::sync::Mutex as StdMutex;
    use tower::ServiceExt;

    #[test]
    fn links_are_normalized() {
        let h = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            normalize_link(h).unwrap(),
            format!("magnet:?xt=urn:btih:{h}")
        );
        assert!(normalize_link(" magnet:?xt=urn:btih:abc ").is_some());
        assert!(normalize_link("https://x/y.torrent").is_some());
        assert!(normalize_link("file:///etc/passwd").is_none());
        assert!(normalize_link("all").is_none());
    }

    #[test]
    fn hashes_reject_wildcards() {
        assert!(!is_hash("all"));
        assert!(is_hash(&"a".repeat(40)));
        assert!(is_hash(&"F".repeat(64)));
        assert!(!is_hash(&"g".repeat(40)));
    }

    // ------------------------------------------------ fake qBittorrent

    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";

    /// Every API path the fake received, in order.
    type Calls = Arc<StdMutex<Vec<String>>>;

    /// A qBittorrent 4.x stand-in: no stop/start endpoints, `Fails.` on bad adds,
    /// 403 until logged in as admin/secret.
    async fn fake_qbit() -> (String, Calls) {
        let calls: Calls = Arc::default();
        let log = calls.clone();

        let app = Router::new().fallback(move |req: Request| {
            let log = log.clone();
            async move {
                let path = req.uri().path().trim_start_matches("/api/v2/").to_owned();
                log.lock().unwrap().push(path.clone());
                let authed = req
                    .headers()
                    .get(header::COOKIE)
                    .is_some_and(|c| c.to_str().unwrap_or("").contains("SID=ok"));
                let body =
                    String::from_utf8_lossy(&req.into_body().collect().await.unwrap().to_bytes())
                        .into_owned();

                match path.as_str() {
                    "auth/login" if body.contains("password=secret") => {
                        ([(header::SET_COOKIE, "SID=ok; path=/")], "Ok.").into_response()
                    }
                    "auth/login" => "Fails.".into_response(),
                    _ if !authed => StatusCode::FORBIDDEN.into_response(),
                    "sync/maindata" => Json(json!({
                        "rid": 1, "full_update": true,
                        "torrents": { HASH: { "name": "debian.iso", "state": "pausedDL",
                                              "progress": 0.5, "magnet_uri": "magnet:?xt=x" } },
                        "categories": { "linux": { "name": "linux" } },
                        "server_state": { "dl_info_speed": 7, "connection_status": "connected" },
                    }))
                    .into_response(),
                    "torrents/add" if body.contains("FAILME") => "Fails.".into_response(),
                    "torrents/add" => "Ok.".into_response(),
                    "torrents/pause" | "torrents/resume" => StatusCode::OK.into_response(),
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
            }
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, calls)
    }

    async fn qbm(qb_pass: &str, ui_password: Option<&str>, base: &str) -> (Router, Calls) {
        let (url, calls) = fake_qbit().await;
        let qb = Qbit::new(url, "admin".into(), qb_pass.into());
        (
            build_router(App::new(qb, ui_password.map(Into::into), base), base),
            calls,
        )
    }

    async fn call(
        router: &Router,
        req: axum::http::Request<Body>,
    ) -> (StatusCode, HeaderMap, String) {
        let res = router.clone().oneshot(req).await.unwrap();
        let (parts, body) = res.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        (
            parts.status,
            parts.headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    fn get(uri: &str) -> axum::http::Request<Body> {
        axum::http::Request::get(uri).body(Body::empty()).unwrap()
    }

    fn post_json(uri: &str, body: Value) -> axum::http::Request<Body> {
        axum::http::Request::post(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn post_multipart(uri: &str, fields: &[(&str, &str)]) -> axum::http::Request<Body> {
        let mut body = String::new();
        for (k, v) in fields {
            body += &format!("--XX\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n");
        }
        body += "--XX--\r\n";
        axum::http::Request::post(uri)
            .header(header::CONTENT_TYPE, "multipart/form-data; boundary=XX")
            .body(Body::from(body))
            .unwrap()
    }

    fn location(h: &HeaderMap) -> &str {
        h.get(header::LOCATION).unwrap().to_str().unwrap()
    }

    // ---------------------------------------------------------- tests

    #[tokio::test]
    async fn base_path_redirects_and_login_keeps_the_incoming_link() {
        let (r, _) = qbm("secret", Some("pw"), "/m").await;

        let (s, h, _) = call(&r, get("/m?add=magnet%3A%3Fxt")).await;
        assert_eq!(s, StatusCode::PERMANENT_REDIRECT);
        assert_eq!(location(&h), "/m/?add=magnet%3A%3Fxt");

        let (s, h, _) = call(&r, get("/m/?add=magnet%3A%3Fxt")).await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        assert_eq!(location(&h), "login?add=magnet%3A%3Fxt");

        let (s, _, body) = call(&r, get("/m/login?add=magnet%3A%3Fxt")).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains(r#"name="q" value="add=magnet%3A%3Fxt""#));

        let (s, _, _) = call(&r, get("/m/api/state")).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, _, _) = call(&r, get("/m/manifest.webmanifest")).await;
        assert_eq!(
            s,
            StatusCode::OK,
            "manifest must be public so Chrome can install"
        );
    }

    #[tokio::test]
    async fn login_sets_a_scoped_cookie_that_unlocks_the_api() {
        let (r, _) = qbm("secret", Some("pw"), "/m").await;
        let login = |pw: &str, https: bool| {
            let mut b = axum::http::Request::post("/m/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            if https {
                b = b.header("x-forwarded-proto", "https");
            }
            b.body(Body::from(format!("password={pw}&q=add%3Dx")))
                .unwrap()
        };

        let (s, h, _) = call(&r, login("nope", false)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert!(h.get(header::SET_COOKIE).is_none());

        let (s, h, _) = call(&r, login("pw", false)).await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        assert_eq!(location(&h), "./?add=x");
        let cookie = h
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert!(cookie.contains("Path=/m/") && !cookie.contains("Secure"));

        let (_, h, _) = call(&r, login("pw", true)).await;
        assert!(
            h.get(header::SET_COOKIE)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("Secure")
        );

        let pair = cookie.split(';').next().unwrap();
        let req = axum::http::Request::get("/m/api/state")
            .header(header::COOKIE, pair)
            .body(Body::empty())
            .unwrap();
        let (s, _, body) = call(&r, req).await;
        assert_eq!(s, StatusCode::OK);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["torrents"][0]["name"], "debian.iso");
        assert_eq!(v["categories"], json!(["linux"]));
        assert!(
            v["torrents"][0].get("magnet_uri").is_none(),
            "poll payload stays small"
        );
    }

    #[tokio::test]
    async fn cross_site_writes_are_refused() {
        let (r, calls) = qbm("secret", None, "").await;
        let mut req = post_json(
            "/api/action",
            json!({ "action": "pause", "hashes": [HASH] }),
        );
        req.headers_mut()
            .insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        let (s, _, _) = call(&r, req).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn add_validates_links_and_reports_rejections() {
        let (r, _) = qbm("secret", None, "").await;

        let (s, _, _) = call(
            &r,
            post_multipart("/api/add", &[("urls", "file:///etc/passwd")]),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        let (s, _, body) = call(
            &r,
            post_multipart("/api/add", &[("urls", "magnet:?xt=FAILME")]),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{body}");

        let (s, _, body) = call(
            &r,
            post_multipart("/api/add", &[("urls", &format!("{HASH}\nmagnet:?xt=b"))]),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["count"], 2);
    }

    #[tokio::test]
    async fn actions_reject_wildcards_and_remember_the_legacy_api() {
        let (r, calls) = qbm("secret", None, "").await;

        let (s, _, _) = call(
            &r,
            post_json(
                "/api/action",
                json!({ "action": "delete", "hashes": ["all"] }),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        for _ in 0..2 {
            let (s, _, _) = call(
                &r,
                post_json(
                    "/api/action",
                    json!({ "action": "pause", "hashes": [HASH] }),
                ),
            )
            .await;
            assert_eq!(s, StatusCode::OK);
        }
        let calls = calls.lock().unwrap();
        let actions: Vec<_> = calls
            .iter()
            .filter(|c| c.starts_with("torrents/"))
            .collect();
        // 403 -> login -> retry 404 -> legacy pause; afterwards straight to pause
        assert_eq!(
            actions,
            [
                "torrents/stop",
                "torrents/stop",
                "torrents/pause",
                "torrents/pause"
            ]
        );
    }

    #[tokio::test]
    async fn wrong_qbittorrent_password_does_not_hammer_the_login() {
        let (r, calls) = qbm("WRONG", None, "").await;
        for _ in 0..5 {
            let (s, _, body) = call(&r, get("/api/state")).await;
            assert_eq!(s, StatusCode::BAD_GATEWAY);
            assert!(body.contains("wrong username or password"));
        }
        let logins = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| *c == "auth/login")
            .count();
        assert_eq!(logins, 1);
    }
}
