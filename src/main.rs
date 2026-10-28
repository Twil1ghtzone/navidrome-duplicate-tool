mod audio;
mod content;
mod engine;
mod index;

use axum::{
    body::Body,
    extract::{Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::Engine as _;
use engine::{Env, Settings, Shared};
use serde::Deserialize;
use serde_json::json;
use std::sync::{atomic::Ordering::Relaxed, Arc};
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");

type App = Arc<Shared>;

/// Konstantzeit-Vergleich (verhindert Timing-Seitenkanal beim Passwort)
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut d = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        d |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    d == 0
}

async fn auth(State(app): State<App>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b.trim()).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .map_or(false, |cred| {
            let expected = format!("{}:{}", app.env.ui_user, app.env.ui_pass);
            ct_eq(cred.as_bytes(), expected.as_bytes())
        });
    if ok {
        return next.run(req).await;
    }
    tokio::time::sleep(Duration::from_millis(600)).await; // Brute-Force bremsen
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"NAVIDROME-DEDUPE\", charset=\"UTF-8\"")],
        "401 ZUGRIFF VERWEIGERT",
    )
        .into_response()
}

/// Nichts aus diesem Dienst darf zwischengespeichert werden. Sonst zeigt der Browser
/// (oder ein Reverse-Proxy) nach einem Container-Update weiter die alte Oberflaeche,
/// obwohl laengst eine neue Version laeuft.
async fn no_cache(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store, no-cache, must-revalidate"));
    h.insert(header::PRAGMA, header::HeaderValue::from_static("no-cache"));
    h.insert(header::EXPIRES, header::HeaderValue::from_static("0"));
    res
}

async fn index(State(app): State<App>) -> Html<String> {
    let page = include_str!("../templates/index.html")
        .replace("{{MODE_EXECUTE}}", if app.execute.load(Relaxed) { "true" } else { "false" })
        .replace("{{DB}}", &app.env.db_path)
        .replace("{{VERSION}}", VERSION)
        .replace("{{KEEP}}", &app.env.keep_exts.join(","))
        .replace("{{DEL}}", &app.env.delete_exts.join(","));
    Html(page)
}

#[derive(Deserialize)]
struct Since {
    since: Option<u64>,
}

/// Warnungen sind teuer (Dateisystem + history.json), deshalb maximal alle 15 Sekunden neu.
fn warnings(app: &App) -> Vec<String> {
    const TTL: i64 = 15;
    let now = chrono::Utc::now().timestamp();
    let mut c = app.warn_cache.lock().unwrap();
    if c.0 + TTL > now {
        return c.1.clone();
    }
    let mut w: Vec<String> = vec![];
    if !std::path::Path::new(&app.env.db_path).is_file() {
        w.push(format!(
            "Navidrome-Datenbank nicht gefunden: {} – das Volume fuer /data muss den Navidrome-Datenordner enthalten.",
            app.env.db_path
        ));
    }
    if app.env.require_dryrun && !app.running.load(Relaxed) && !engine::recent_dry_run(&app.env) {
        w.push("Fuer AUSFUEHREN ist zuerst ein vollstaendiger Dry-Run noetig (letzte 7 Tage).".into());
    }
    *c = (now, w.clone());
    w
}

async fn status(State(app): State<App>, Query(q): Query<Since>) -> Json<serde_json::Value> {
    let (next, lines): (u64, Vec<(u64, String)>) = {
        let g = app.log.lock().unwrap();
        let since = q.since.unwrap_or(0);
        (g.0, g.1.iter().filter(|(i, _)| *i > since).cloned().collect())
    };
    let running = app.running.load(Relaxed);
    let stats = app.stats_json();
    // Laufzeit, Durchsatz und geschaetzte Restzeit serverseitig – die UI muss nichts rechnen
    let started = app.started_ts.load(Relaxed);
    let ended = app.ended_ts.load(Relaxed);
    let now = chrono::Utc::now().timestamp().max(0) as u64;
    // nach dem Lauf bleibt die Laufzeit stehen statt weiterzuzaehlen
    let elapsed = if started == 0 { 0 } else { if ended > 0 { ended } else { now }.saturating_sub(started) };
    let scanned = stats["scanned"].as_u64().unwrap_or(0);
    let total = stats["total"].as_u64().unwrap_or(0);
    let rate = if elapsed > 0 { scanned as f64 / elapsed as f64 } else { 0.0 };
    let eta = if running && rate > 0.5 && total > scanned { Some(((total - scanned) as f64 / rate) as u64) } else { None };
    Json(json!({
        "version": VERSION,
        "running": running,
        "execute": app.execute.load(Relaxed),
        "phase": *app.phase.lock().unwrap(),
        "stats": stats,
        "elapsed": elapsed,
        "rate": (rate * 10.0).round() / 10.0,
        "eta": eta,
        "warnings": warnings(&app),
        "next": next,
        "log": lines,
    }))
}

#[derive(Deserialize)]
struct StartReq {
    execute: bool,
}

fn spawn_run(app: &App, execute: bool) -> bool {
    if app.running.swap(true, Relaxed) {
        return false;
    }
    let a = app.clone();
    std::thread::spawn(move || engine::run(&a, execute));
    true
}

async fn start(State(app): State<App>, Json(r): Json<StartReq>) -> impl IntoResponse {
    if r.execute && app.env.require_dryrun && !engine::recent_dry_run(&app.env) {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(json!({"ok": false, "error": "Sicherheitssperre: Bitte zuerst einen vollstaendigen Dry-Run durchfuehren (nicht aelter als 7 Tage, nicht vorzeitig gestoppt)."})),
        );
    }
    if spawn_run(&app, r.execute) {
        (StatusCode::OK, Json(json!({"ok": true})))
    } else {
        (StatusCode::CONFLICT, Json(json!({"ok": false, "error": "laeuft bereits"})))
    }
}

async fn stop(State(app): State<App>) -> Json<serde_json::Value> {
    app.stop.store(true, Relaxed);
    app.log("STOP angefordert – aktueller Block wird sauber abgeschlossen ...");
    Json(json!({"ok": true}))
}

async fn get_cfg(State(app): State<App>) -> Json<Settings> {
    Json(app.settings.lock().unwrap().clone())
}

async fn set_cfg(State(app): State<App>, Json(mut s): Json<Settings>) -> impl IntoResponse {
    if app.running.load(Relaxed) {
        return (StatusCode::CONFLICT, "Konfiguration waehrend eines Laufs gesperrt").into_response();
    }
    for v in s.excludes.iter_mut().chain(s.keywords.iter_mut()) {
        *v = v.trim().to_string();
    }
    s.mode = s.mode.trim().to_lowercase();
    if !["db", "content", "both"].contains(&s.mode.as_str()) {
        return (StatusCode::BAD_REQUEST, "Unbekanntes Suchverfahren").into_response();
    }
    if !(0.0..=60.0).contains(&s.duration_tol) {
        return (StatusCode::BAD_REQUEST, "Laengen-Toleranz muss zwischen 0 und 60 Sekunden liegen").into_response();
    }
    s.excludes.retain(|v| !v.is_empty());
    s.keywords.retain(|v| !v.is_empty());
    if let Err(e) = engine::save_settings(&app.env, &s) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("speichern fehlgeschlagen: {e}")).into_response();
    }
    *app.settings.lock().unwrap() = s;
    app.log("Konfiguration gespeichert");
    StatusCode::OK.into_response()
}

async fn logs_zip(State(app): State<App>) -> Response {
    let a = app.clone();
    match tokio::task::spawn_blocking(move || engine::build_zip(&a)).await {
        Ok(Ok(file)) => {
            let stream = tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file));
            let name = format!("navidrome-dedupe-logs-{}.zip", chrono::Local::now().format("%Y%m%d-%H%M%S"));
            (
                [
                    (header::CONTENT_TYPE, "application/zip".to_string()),
                    (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
                ],
                Body::from_stream(stream),
            )
                .into_response()
        }
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("ZIP fehlgeschlagen: {e}")).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("ZIP fehlgeschlagen: {e}")).into_response(),
    }
}

async fn history(State(app): State<App>) -> Json<serde_json::Value> {
    let mut h = engine::history(&app.env);
    h.reverse();
    Json(json!({"history": h, "backups": engine::list_backups(&app.env)}))
}

async fn backup_now(State(app): State<App>) -> impl IntoResponse {
    let a = app.clone();
    match tokio::task::spawn_blocking(move || engine::manual_backup(&a)).await {
        Ok(Ok(p)) => {
            app.log(format!("Manuelles Backup erstellt: {p}"));
            (StatusCode::OK, Json(json!({"ok": true, "path": p})))
        }
        Ok(Err(e)) => (StatusCode::CONFLICT, Json(json!({"ok": false, "error": e}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e.to_string()}))),
    }
}

#[derive(Deserialize)]
struct IndexQuery {
    q: Option<String>,
    only: Option<String>,
    ext: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// Durchsucht die eigene Index-Datenbank. Laeuft auf einer Nur-Lese-Verbindung,
/// stoert also keinen laufenden Scan.
async fn index_search(State(app): State<App>, Query(q): Query<IndexQuery>) -> impl IntoResponse {
    let r = tokio::task::spawn_blocking(move || {
        let c = engine_index_open(&app)?;
        index::search(
            &c,
            q.q.as_deref().unwrap_or(""),
            q.only.as_deref().unwrap_or("all"),
            q.ext.as_deref().unwrap_or(""),
            q.limit.unwrap_or(50),
            q.offset.unwrap_or(0),
        )
    })
    .await;
    match r {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)),
        Ok(Err(e)) => (StatusCode::NOT_FOUND, Json(json!({"error": e}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

async fn index_summary(State(app): State<App>) -> impl IntoResponse {
    let r = tokio::task::spawn_blocking(move || engine_index_open(&app).and_then(|c| index::summary(&c))).await;
    match r {
        Ok(Ok(v)) => (StatusCode::OK, Json(v)),
        Ok(Err(e)) => (StatusCode::NOT_FOUND, Json(json!({"error": e}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

async fn index_reset(State(app): State<App>) -> impl IntoResponse {
    if app.running.load(Relaxed) {
        return (StatusCode::CONFLICT, Json(json!({"ok": false, "error": "Waehrend eines Laufs gesperrt"})));
    }
    match index::reset(&app.env) {
        Ok(()) => {
            app.log("Index verworfen – der naechste Lauf baut ihn neu auf");
            (StatusCode::OK, Json(json!({"ok": true})))
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e}))),
    }
}

fn engine_index_open(app: &App) -> Result<rusqlite::Connection, String> {
    index::open_ro(&app.env)
}

async fn healthz() -> String {
    format!("ok navidrome-dedupe {VERSION}")
}

async fn shutdown_signal() {
    let ctrl_c = async { let _ = tokio::signal::ctrl_c().await; };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());

    if args.first().map(String::as_str) == Some("healthcheck") {
        use std::io::{Read, Write};
        let port = bind.rsplit(':').next().unwrap_or("8080");
        let ok = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).and_then(|mut c| {
            c.set_read_timeout(Some(Duration::from_secs(2)))?;
            c.write_all(b"GET /healthz HTTP/1.0\r\n\r\n")?;
            let mut b = String::new();
            c.read_to_string(&mut b)?;
            Ok(b.contains("200"))
        });
        std::process::exit(if ok.unwrap_or(false) { 0 } else { 1 });
    }

    let env = match Env::from_env() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Konfigurationsfehler: {e}");
            std::process::exit(2);
        }
    };
    if env.generated_pass {
        eprintln!("==============================================================");
        eprintln!(" UI_PASSWORD war nicht gesetzt. Generiertes Passwort fuer '{}':", env.ui_user);
        eprintln!("   {}", env.ui_pass);
        eprintln!(" (gespeichert in {}/.ui_password)", env.config_dir.display());
        eprintln!("==============================================================");
    }
    let execute_default = args.iter().any(|a| a == "--execute");
    let headless = args.iter().any(|a| a == "--headless");
    let app: App = Arc::new(Shared::new(env));
    app.execute.store(execute_default && !args.iter().any(|a| a == "--dry-run"), Relaxed);

    if headless {
        // Einmaliger Lauf ohne UI: `--headless [--execute]`
        app.running.store(true, Relaxed);
        let a = app.clone();
        let ex = app.execute.load(Relaxed);
        let h = std::thread::spawn(move || engine::run(&a, ex));
        let _ = h.join();
        std::process::exit(if app.stats.errors.load(Relaxed) == 0 { 0 } else { 1 });
    }

    let protected = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/start", post(start))
        .route("/api/stop", post(stop))
        .route("/api/config", get(get_cfg).post(set_cfg))
        .route("/api/logs.zip", get(logs_zip))
        .route("/api/history", get(history))
        .route("/api/backup", post(backup_now))
        .route("/api/index", get(index_search))
        .route("/api/index/summary", get(index_summary))
        .route("/api/index/reset", post(index_reset))
        .layer(middleware::from_fn_with_state(app.clone(), auth));
    let router = Router::new()
        .route("/healthz", get(healthz))
        .merge(protected)
        .layer(middleware::from_fn(no_cache))
        .with_state(app.clone());

    let listener = tokio::net::TcpListener::bind(&bind).await.unwrap_or_else(|e| {
        eprintln!("bind {bind}: {e}");
        std::process::exit(2);
    });
    eprintln!("navidrome-dedupe {VERSION} – Web-UI auf http://{bind} (DB: {})", app.env.db_path);

    let a = app.clone();
    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        shutdown_signal().await;
        a.log("SIGTERM/SIGINT – beende nach aktuellem Block ...");
        a.shutdown.store(true, Relaxed);
    });
    let _ = server.await;

    // Auf sauberes Ende des laufenden Blocks warten (Transaktion nie hart abbrechen)
    while app.running.load(Relaxed) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    eprintln!("Sauber beendet.");
}
