mod engine;

use axum::{
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

async fn index(State(app): State<App>) -> Html<String> {
    let page = include_str!("../templates/index.html")
        .replace("{{MODE_EXECUTE}}", if app.execute.load(Relaxed) { "true" } else { "false" })
        .replace("{{DB}}", &app.env.db_path)
        .replace("{{KEEP}}", &app.env.keep_exts.join(","))
        .replace("{{DEL}}", &app.env.delete_exts.join(","));
    Html(page)
}

#[derive(Deserialize)]
struct Since {
    since: Option<u64>,
}

async fn status(State(app): State<App>, Query(q): Query<Since>) -> Json<serde_json::Value> {
    let st = &app.stats;
    let (next, lines): (u64, Vec<(u64, String)>) = {
        let g = app.log.lock().unwrap();
        let since = q.since.unwrap_or(0);
        (g.0, g.1.iter().filter(|(i, _)| *i > since).cloned().collect())
    };
    Json(json!({
        "running": app.running.load(Relaxed),
        "execute": app.execute.load(Relaxed),
        "phase": *app.phase.lock().unwrap(),
        "stats": {
            "total": st.total.load(Relaxed), "scanned": st.scanned.load(Relaxed),
            "candidates": st.candidates.load(Relaxed), "duplicates": st.duplicates.load(Relaxed),
            "protected": st.protected.load(Relaxed), "excluded": st.excluded.load(Relaxed),
            "mismatch": st.mismatch.load(Relaxed), "deleted": st.deleted.load(Relaxed),
            "bytes_freed": st.bytes_freed.load(Relaxed), "annotations": st.annotations.load(Relaxed),
            "errors": st.errors.load(Relaxed)
        },
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
    s.excludes.retain(|v| !v.is_empty());
    s.keywords.retain(|v| !v.is_empty());
    if let Err(e) = engine::save_settings(&app.env, &s) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("speichern fehlgeschlagen: {e}")).into_response();
    }
    *app.settings.lock().unwrap() = s;
    app.log("Konfiguration gespeichert");
    StatusCode::OK.into_response()
}

async fn healthz() -> &'static str {
    "ok"
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
        .layer(middleware::from_fn_with_state(app.clone(), auth));
    let router = Router::new().route("/healthz", get(healthz)).merge(protected).with_state(app.clone());

    let listener = tokio::net::TcpListener::bind(&bind).await.unwrap_or_else(|e| {
        eprintln!("bind {bind}: {e}");
        std::process::exit(2);
    });
    app.log(format!("Web-UI auf http://{bind}  (DB: {})", app.env.db_path));

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
