//! Dedupe-Engine für Navidrome.
//!
//! Ablauf in zwei Phasen, damit auch Bibliotheken mit Millionen Titeln
//! mit konstantem Speicher und ohne dauerhaft offene Lese-Transaktion laufen:
//!
//! 1. **Planen** (nur lesend): `media_file` wird sortiert gestreamt, Gruppen
//!    werden gebildet, Schutzregeln geprüft und jede geplante Löschung in eine
//!    Plan-Datei (= Report) geschrieben. Die Lese-Transaktion ist pro Block kurz.
//! 2. **Anwenden** (nur im Execute-Modus): die Plan-Datei wird blockweise
//!    abgearbeitet. Pro Block eine SQLite-Transaktion, in der alle
//!    nutzerbezogenen Daten (Favoriten, Bewertungen, Wiedergaben, Lesezeichen,
//!    Playlists, Scrobbles, Warteschlangen, Shares) auf die behaltene Datei
//!    übertragen werden. Dateien werden erst NACH dem Commit gelöscht.
//!
//! Das Schema wird zur Laufzeit per `PRAGMA table_info` ermittelt, damit das
//! Tool über verschiedene Navidrome-Versionen hinweg funktioniert.

use base64::Engine as _;
use rusqlite::{params, Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

// ───────────────────────── Konfiguration ─────────────────────────

#[derive(Clone, Copy, PartialEq)]
pub enum MatchMode {
    Album,       // gleiche album_id + Disc + Titel (sicher, Standard)
    ArtistTitle, // gleicher Artist + Titel (albumübergreifend, riskanter)
}

#[derive(Clone)]
pub struct Env {
    pub db_path: String,
    pub backup_dir: PathBuf,
    pub config_dir: PathBuf,
    pub path_map: Option<(String, String)>,
    pub keep_exts: Vec<String>,
    pub delete_exts: Vec<String>,
    pub same_exts: Vec<String>,
    pub duration_tol: f64,
    pub batch_size: usize,
    pub backup_keep: usize,
    pub match_mode: MatchMode,
    pub delete_rows: bool,
    pub ui_user: String,
    pub ui_pass: String,
    pub generated_pass: bool,
    pub require_dryrun: bool,
    pub log_keep: usize,
}

fn var(k: &str, d: &str) -> String {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| d.to_string())
}
fn list(k: &str, d: &str) -> Vec<String> {
    var(k, d)
        .split(',')
        .map(|s| s.trim().trim_start_matches('.').to_lowercase())
        .filter(|s| !s.is_empty() && s != "none" && s.chars().all(|c| c.is_ascii_alphanumeric()))
        .collect()
}

fn auto_password(dir: &Path) -> Result<String, String> {
    let f = dir.join(".ui_password");
    if let Ok(p) = fs::read_to_string(&f) {
        let p = p.trim().to_string();
        if !p.is_empty() {
            return Ok(p);
        }
    }
    let mut buf = [0u8; 18];
    fs::File::open("/dev/urandom")
        .and_then(|mut r| std::io::Read::read_exact(&mut r, &mut buf))
        .map_err(|e| format!("kein Zufall fuer Passwort-Generierung: {e} – bitte UI_PASSWORD setzen"))?;
    let p = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf);
    fs::create_dir_all(dir).map_err(|e| format!("CONFIG_DIR {} nicht beschreibbar: {e}", dir.display()))?;
    fs::write(&f, &p).map_err(|e| format!("Passwort speichern: {e} – bitte UI_PASSWORD setzen"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&f, fs::Permissions::from_mode(0o600));
    }
    Ok(p)
}

impl Env {
    pub fn from_env() -> Result<Env, String> {
        let ui_user = var("UI_USER", "admin");
        let config_dir: PathBuf = var("CONFIG_DIR", "/config").into();
        let mut ui_pass = var("UI_PASSWORD", "");
        let mut generated_pass = false;
        if ui_pass.is_empty() {
            ui_pass = auto_password(&config_dir)?;
            generated_pass = true;
        }
        let path_map = std::env::var("PATH_MAP").ok().and_then(|v| {
            v.split_once(':').map(|(a, b)| (a.trim_end_matches('/').to_string(), b.trim_end_matches('/').to_string()))
        });
        Ok(Env {
            db_path: var("NAVIDROME_DB", "/data/navidrome.db"),
            backup_dir: var("BACKUP_DIR", "/backups").into(),
            config_dir,
            path_map,
            keep_exts: list("KEEP_EXTS", "flac"),
            delete_exts: list("DELETE_EXTS", "mp3"),
            same_exts: list("DEDUPE_SAME_EXTS", "flac"),
            duration_tol: var("DURATION_TOLERANCE", "3").parse().unwrap_or(3.0),
            batch_size: var("BATCH_SIZE", "200").parse().unwrap_or(200).max(1),
            backup_keep: var("BACKUP_KEEP", "3").parse().unwrap_or(3),
            match_mode: if var("MATCH_MODE", "album") == "artist_title" { MatchMode::ArtistTitle } else { MatchMode::Album },
            delete_rows: var("DELETE_ROWS", "false") == "true",
            ui_user,
            ui_pass,
            generated_pass,
            require_dryrun: var("REQUIRE_DRYRUN", "true") != "false",
            log_keep: var("LOG_KEEP", "30").parse().unwrap_or(30),
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub excludes: Vec<String>,
    pub keywords: Vec<String>,
    /// 0 = unbegrenzt; sonst stoppt der Lauf nach so vielen erkannten Duplikaten
    pub max_delete: u64,
    /// doppelte Dateien gleichen Formats (DEDUPE_SAME_EXTS, z. B. FLAC) ebenfalls bereinigen
    pub dedupe_flac: bool,
    pub duration_tol: f64,
    /// Playlist-Eintraege werden 1:1 auf die behaltene Datei umgebogen – auch wenn
    /// diese dadurch doppelt in der Playlist steht. Garantiert, dass die Playlist
    /// weder kuerzer wird noch Positionen verschiebt.
    pub playlist_keep_duplicates: bool,
    /// Haben beide Dateien eine MusicBrainz-Recording-ID und sind diese verschieden,
    /// gilt das als "nicht dasselbe Stueck" und es wird nichts geloescht.
    pub require_mbz_match: bool,
    /// Bei gleichem Format muss die Tracknummer uebereinstimmen.
    pub require_same_track: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Settings {
            excludes: vec![],
            keywords: ["instrumental", "karaoke", "acapella", "a cappella", "a capella", "accapella"]
                .iter().map(|s| s.to_string()).collect(),
            max_delete: 0,
            dedupe_flac: true,
            duration_tol: 3.0,
            playlist_keep_duplicates: true,
            require_mbz_match: true,
            require_same_track: true,
        }
    }
}

// ───────────────────────── Gemeinsamer Zustand ─────────────────────────

#[derive(Default)]
pub struct Stats {
    pub total: AtomicU64,
    pub scanned: AtomicU64,
    pub candidates: AtomicU64,
    pub duplicates: AtomicU64,
    pub same_format: AtomicU64,
    pub protected: AtomicU64,
    pub excluded: AtomicU64,
    pub mismatch: AtomicU64,
    pub deleted: AtomicU64,
    pub bytes_freed: AtomicU64,
    pub annotations: AtomicU64,
    pub playlists: AtomicU64,
    pub errors: AtomicU64,
}

pub struct Shared {
    pub env: Env,
    pub settings: Mutex<Settings>,
    pub stats: Stats,
    pub running: AtomicBool,
    pub stop: AtomicBool,
    pub shutdown: AtomicBool,
    /// Lauf-Limit erreicht – der Plan ist vollstaendig und darf angewendet werden
    pub limit: AtomicBool,
    pub execute: AtomicBool,
    pub phase: Mutex<String>,
    pub log: Mutex<(u64, VecDeque<(u64, String)>)>,
    pub logfile: Mutex<Option<fs::File>>,
}

impl Shared {
    pub fn new(env: Env) -> Shared {
        let settings = load_settings(&env);
        Shared {
            env,
            settings: Mutex::new(settings),
            stats: Stats::default(),
            running: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            limit: AtomicBool::new(false),
            execute: AtomicBool::new(false),
            phase: Mutex::new("bereit".into()),
            log: Mutex::new((0, VecDeque::new())),
            logfile: Mutex::new(None),
        }
    }
    pub fn log(&self, msg: impl AsRef<str>) {
        let line = format!("[{}] {}", chrono::Local::now().format("%H:%M:%S"), msg.as_ref());
        eprintln!("{line}");
        if let Some(f) = self.logfile.lock().unwrap().as_mut() {
            let _ = writeln!(f, "{line}");
        }
        let mut g = self.log.lock().unwrap();
        g.0 += 1;
        let seq = g.0;
        g.1.push_back((seq, line));
        while g.1.len() > 1000 {
            g.1.pop_front();
        }
    }
    pub fn stats_json(&self) -> Value {
        let st = &self.stats;
        json!({
            "total": st.total.load(Relaxed), "scanned": st.scanned.load(Relaxed),
            "candidates": st.candidates.load(Relaxed), "duplicates": st.duplicates.load(Relaxed),
            "same_format": st.same_format.load(Relaxed),
            "protected": st.protected.load(Relaxed), "excluded": st.excluded.load(Relaxed),
            "mismatch": st.mismatch.load(Relaxed), "deleted": st.deleted.load(Relaxed),
            "bytes_freed": st.bytes_freed.load(Relaxed), "annotations": st.annotations.load(Relaxed),
            "playlists": st.playlists.load(Relaxed),
            "errors": st.errors.load(Relaxed)
        })
    }
    fn phase(&self, p: &str) {
        *self.phase.lock().unwrap() = p.to_string();
    }
    fn reset_stats(&self) {
        let s = &self.stats;
        for a in [&s.total, &s.scanned, &s.candidates, &s.duplicates, &s.same_format, &s.protected,
                  &s.excluded, &s.mismatch, &s.deleted, &s.bytes_freed, &s.annotations, &s.playlists, &s.errors] {
            a.store(0, Relaxed);
        }
    }
    /// Abbruch durch Nutzer oder Signal – dann wird NICHTS mehr angewendet.
    fn halt(&self) -> bool {
        self.stop.load(Relaxed) || self.shutdown.load(Relaxed)
    }
    /// Scan-Ende: Abbruch oder Lauf-Limit.
    fn scan_done(&self) -> bool {
        self.halt() || self.limit.load(Relaxed)
    }
}

fn settings_path(env: &Env) -> PathBuf {
    env.config_dir.join("settings.json")
}
fn load_settings(env: &Env) -> Settings {
    fs::read_to_string(settings_path(env)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| {
        let mut s = Settings::default();
        s.duration_tol = env.duration_tol;
        s.dedupe_flac = !env.same_exts.is_empty();
        s
    })
}
pub fn save_settings(env: &Env, s: &Settings) -> std::io::Result<()> {
    fs::create_dir_all(&env.config_dir)?;
    fs::write(settings_path(env), serde_json::to_string_pretty(s).unwrap())
}

// ───────────────────────── Schema-Erkennung ─────────────────────────

fn cols(c: &Connection, table: &str) -> Vec<String> {
    let mut st = match c.prepare(&format!("PRAGMA table_info(\"{}\")", table.replace('"', ""))) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    st.query_map([], |r| r.get::<_, String>(1))
        .map(|it| it.filter_map(|x| x.ok()).collect())
        .unwrap_or_default()
}

/// Welche Tabellen/Spalten hat diese Navidrome-Datenbank?
pub struct Schema {
    mf: HashSet<String>,
    ann: HashSet<String>,
    playlist: HashSet<String>,
    /// Lesezeichen-Spalten ausser dem Schluessel (user_id, item_id, item_type)
    bookmark: Vec<String>,
    tables: HashSet<String>,
}

impl Schema {
    fn load(c: &Connection) -> Result<Schema, String> {
        let mut st = c
            .prepare("SELECT name FROM sqlite_master WHERE type IN ('table','view')")
            .map_err(|e| e.to_string())?;
        let tables: HashSet<String> = st
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .filter_map(|x| x.ok())
            .collect();
        for t in ["media_file", "annotation", "library"] {
            if !tables.contains(t) {
                return Err(format!(
                    "Das ist keine gueltige Navidrome-Datenbank (Tabelle '{t}' fehlt) – Volume fuer /data pruefen"
                ));
            }
        }
        let key = ["user_id", "item_id", "item_type"];
        Ok(Schema {
            mf: cols(c, "media_file").into_iter().collect(),
            ann: cols(c, "annotation").into_iter().collect(),
            playlist: cols(c, "playlist").into_iter().collect(),
            bookmark: cols(c, "bookmark").into_iter().filter(|x| !key.contains(&x.as_str())).collect(),
            tables,
        })
    }
    fn mf(&self, c: &str) -> bool {
        self.mf.contains(c)
    }
    fn table(&self, t: &str) -> bool {
        self.tables.contains(t)
    }
    /// Spaltenausdruck fuer das SELECT: existiert die Spalte nicht, liefert `fallback`.
    fn expr(&self, col: &str, fallback: &str) -> String {
        if self.mf(col) { col.to_string() } else { fallback.to_string() }
    }
}

// ───────────────────────── Datenmodell ─────────────────────────

#[derive(Clone)]
struct Row {
    id: String,
    library_id: i64,
    path: String,
    title: String,
    album: String,
    suffix: String,
    size: i64,
    duration: f64,
    bit_rate: i64,
    sample_rate: i64,
    bit_depth: i64,
    channels: i64,
    mbz: String,
    comment: String,
    tags: String,
    track: i64,
}

/// Eine geplante Loeschung (Zeile der Plan-Datei).
#[derive(Clone)]
struct PlanRow {
    keeper_id: String,
    keeper_path: String,
    del_id: String,
    del_path: String,
    size: i64,
    reason: String,
}

fn s(r: &rusqlite::Row, i: usize) -> String {
    r.get::<_, Option<String>>(i).ok().flatten().unwrap_or_default()
}
fn n(r: &rusqlite::Row, i: usize) -> i64 {
    r.get::<_, Option<i64>>(i).ok().flatten().unwrap_or(0)
}
fn f(r: &rusqlite::Row, i: usize) -> f64 {
    r.get::<_, Option<f64>>(i).ok().flatten().unwrap_or(0.0)
}

// ───────────────────────── Einstieg ─────────────────────────

pub fn run(sh: &Shared, execute: bool) {
    sh.stop.store(false, Relaxed);
    sh.limit.store(false, Relaxed);
    sh.execute.store(execute, Relaxed);
    sh.reset_stats();
    let started = chrono::Local::now();
    let mode = if execute { "execute" } else { "dry-run" };
    let logname = format!("{}-{mode}.log", started.format("%Y%m%d-%H%M%S"));
    let ldir = sh.env.config_dir.join("logs");
    let _ = fs::create_dir_all(&ldir);
    *sh.logfile.lock().unwrap() = fs::File::create(ldir.join(&logname)).ok();
    sh.log(format!("=== START ({}) ===", if execute { "EXECUTE – echte Loeschung" } else { "DRY-RUN – es wird nichts veraendert" }));
    let (result, completed) = match run_inner(sh, execute) {
        Ok(()) if sh.halt() => ("gestoppt", false),
        Ok(()) if sh.limit.load(Relaxed) => ("limit", false),
        Ok(()) => ("fertig", true),
        Err(e) => {
            sh.stats.errors.fetch_add(1, Relaxed);
            sh.log(format!("FEHLER: {e}"));
            ("fehler", false)
        }
    };
    sh.phase(result);
    sh.log(format!("=== ENDE: {result} ==="));
    let ended = chrono::Local::now();
    add_history(
        &sh.env,
        json!({
            "start": started.format("%Y-%m-%d %H:%M:%S").to_string(), "end": ended.format("%Y-%m-%d %H:%M:%S").to_string(),
            "end_ts": chrono::Utc::now().timestamp(), "mode": mode, "result": result, "completed": completed,
            "log": logname, "stats": sh.stats_json(),
        }),
    );
    *sh.logfile.lock().unwrap() = None;
    rotate(&ldir, sh.env.log_keep);
    rotate(&sh.env.config_dir.join("reports"), sh.env.log_keep * 2);
    sh.running.store(false, Relaxed);
}

fn rotate(dir: &Path, keep: usize) {
    if let Ok(rd) = fs::read_dir(dir) {
        let mut v: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file()).collect();
        v.sort();
        while v.len() > keep.max(1) {
            let _ = fs::remove_file(v.remove(0));
        }
    }
}

// ───────────────────────── Verlauf, Backups, Log-ZIP ─────────────────────────

fn history_path(env: &Env) -> PathBuf {
    env.config_dir.join("history.json")
}
pub fn history(env: &Env) -> Vec<Value> {
    fs::read_to_string(history_path(env)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
fn add_history(env: &Env, entry: Value) {
    let mut h = history(env);
    h.push(entry);
    let n = h.len();
    if n > 100 {
        h.drain(0..n - 100);
    }
    let _ = fs::write(history_path(env), serde_json::to_string_pretty(&h).unwrap_or_default());
}
/// Sicherheitsgurt: Execute nur nach vollstaendigem Dry-Run der letzten 7 Tage.
pub fn recent_dry_run(env: &Env) -> bool {
    let min = chrono::Utc::now().timestamp() - 7 * 86400;
    history(env).iter().any(|e| {
        e["mode"] == "dry-run" && e["completed"] == true && e["end_ts"].as_i64().unwrap_or(0) >= min
    })
}

pub fn list_backups(env: &Env) -> Vec<Value> {
    let mut v: Vec<(String, u64)> = fs::read_dir(&env.backup_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    (n.starts_with("navidrome-backup-") && n.ends_with(".db")).then(|| (n, e.metadata().map(|m| m.len()).unwrap_or(0)))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v.reverse();
    v.into_iter().map(|(n, s)| json!({"name": n, "size": s})).collect()
}

pub fn manual_backup(sh: &Shared) -> Result<String, String> {
    if sh.running.swap(true, Relaxed) {
        return Err("Es laeuft gerade ein Scan".into());
    }
    let r = open(&sh.env, false).and_then(|w| backup(sh, &w));
    sh.running.store(false, Relaxed);
    r.map(|p| p.display().to_string())
}

/// Alle Logs + Reports + Verlauf als ZIP (temporaere Datei wird sofort ausgehaengt).
pub fn build_zip(sh: &Shared) -> std::io::Result<fs::File> {
    use std::io::{Seek, SeekFrom};
    use zip::write::SimpleFileOptions;
    let env = &sh.env;
    fs::create_dir_all(&env.config_dir)?;
    let tmp = env.config_dir.join(format!(".tmp-{}.zip", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)));
    let file = fs::OpenOptions::new().read(true).write(true).create_new(true).open(&tmp)?;
    let _ = fs::remove_file(&tmp);
    let mut zw = zip::ZipWriter::new(file);
    let opt = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).large_file(true);
    let info = format!(
        "navidrome-dedupe {}\ndb: {}\nkeep: {:?}\ndelete: {:?}\nsame: {:?}\nmatch_mode: {}\ndelete_rows: {}\nsettings: {}\n",
        env!("CARGO_PKG_VERSION"),
        env.db_path,
        env.keep_exts,
        env.delete_exts,
        env.same_exts,
        if env.match_mode == MatchMode::Album { "album" } else { "artist_title" },
        env.delete_rows,
        serde_json::to_string(&*sh.settings.lock().unwrap()).unwrap_or_default()
    );
    zw.start_file("system-info.txt", opt)?;
    zw.write_all(info.as_bytes())?;
    for dir in ["logs", "reports"] {
        if let Ok(rd) = fs::read_dir(env.config_dir.join(dir)) {
            let mut v: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file()).collect();
            v.sort();
            for p in v {
                let name = p.file_name().unwrap().to_string_lossy().to_string();
                zw.start_file(format!("{dir}/{name}"), opt)?;
                std::io::copy(&mut fs::File::open(&p)?, &mut zw)?;
            }
        }
    }
    for n in ["history.json", "settings.json"] {
        if let Ok(mut f) = fs::File::open(env.config_dir.join(n)) {
            zw.start_file(n, opt)?;
            std::io::copy(&mut f, &mut zw)?;
        }
    }
    let mut file = zw.finish()?;
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

fn open(env: &Env, query_only: bool) -> Result<Connection, String> {
    let c = Connection::open_with_flags(
        &env.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("DB oeffnen ({}): {e} – Pfad/Volume pruefen, es wird absichtlich KEINE neue DB angelegt", env.db_path))?;
    c.busy_timeout(std::time::Duration::from_secs(60)).map_err(|e| e.to_string())?;
    if query_only {
        c.execute_batch("PRAGMA query_only=ON; PRAGMA cache_size=-65536;").map_err(|e| e.to_string())?;
    } else {
        c.execute_batch("PRAGMA foreign_keys=ON;").map_err(|e| e.to_string())?;
    }
    Ok(c)
}

fn backup(sh: &Shared, w: &Connection) -> Result<PathBuf, String> {
    let env = &sh.env;
    fs::create_dir_all(&env.backup_dir).map_err(|e| format!("Backup-Dir: {e}"))?;
    let name = format!("navidrome-backup-{}.db", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let target = env.backup_dir.join(&name);
    w.execute(&format!("VACUUM INTO '{}'", target.to_string_lossy().replace('\'', "''")), [])
        .map_err(|e| format!("Backup fehlgeschlagen (Abbruch, nichts veraendert): {e}"))?;
    if fs::metadata(&target).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err("Backup-Datei leer – Abbruch".into());
    }
    let mut all: Vec<PathBuf> = fs::read_dir(&env.backup_dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().and_then(|n| n.to_str()).map_or(false, |n| n.starts_with("navidrome-backup-") && n.ends_with(".db")))
        .collect();
    all.sort();
    while all.len() > env.backup_keep.max(1) {
        let old = all.remove(0);
        let _ = fs::remove_file(&old);
    }
    Ok(target)
}

// ───────────────────────── Lauf ─────────────────────────

fn run_inner(sh: &Shared, execute: bool) -> Result<(), String> {
    let env = &sh.env;
    let settings = sh.settings.lock().unwrap().clone();
    if env.keep_exts.is_empty() {
        return Err("KEEP_EXTS ist leer – ohne zu behaltendes Format kann nichts entschieden werden".into());
    }
    let same_exts: Vec<String> = if settings.dedupe_flac { env.same_exts.clone() } else { vec![] };
    if env.delete_exts.is_empty() && same_exts.is_empty() {
        return Err("Weder DELETE_EXTS noch die Bereinigung gleicher Formate ist aktiv – es gaebe nichts zu loeschen".into());
    }

    // ── Phase 1: planen (nur lesend) ──
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let rdir = env.config_dir.join("reports");
    fs::create_dir_all(&rdir).map_err(|e| format!("Report-Ordner {}: {e}", rdir.display()))?;
    let plan_path = rdir.join(format!("{ts}-{}-plan.tsv", if execute { "execute" } else { "dryrun" }));

    let planned = {
        let reader = open(env, true)?;
        let schema = Schema::load(&reader)?;
        let libs = libraries(sh, &reader)?;
        sh.log(format!(
            "Schema: rated_at={} bookmark={} playqueue={} scrobbles={} scrobble_buffer={} share={} playlists={}",
            schema.ann.contains("rated_at"), schema.table("bookmark"), schema.table("playqueue"),
            schema.table("scrobbles"), schema.table("scrobble_buffer"), schema.table("share"),
            schema.table("playlist_tracks")
        ));
        sh.log(format!("Report/Plan: {}", plan_path.display()));
        let plan = fs::File::create(&plan_path).map_err(|e| format!("Plan-Datei: {e}"))?;
        let mut plan = BufWriter::new(plan);
        writeln!(plan, "#keeper_id\tkeeper_path\tdel_id\tdel_path\tdel_size\treason").map_err(|e| e.to_string())?;
        let n = scan(sh, &reader, &schema, &settings, &libs, &same_exts, &mut plan)?;
        plan.flush().map_err(|e| e.to_string())?;
        n
    }; // Reader hier geschlossen: ab jetzt blockiert keine Lese-Transaktion den WAL-Checkpoint

    sh.log(format!("Plan fertig: {planned} Loeschungen vorgemerkt"));
    if !execute {
        return Ok(());
    }
    if sh.halt() {
        sh.log("Abbruch angefordert – es wird nichts geloescht");
        return Ok(());
    }
    if planned == 0 {
        return Ok(());
    }

    // ── Phase 2: anwenden ──
    let mut w = open(env, false)?;
    let schema = Schema::load(&w)?;
    sh.phase("Backup");
    let bk = backup(sh, &w)?;
    sh.log(format!("Backup erstellt: {}", bk.display()));
    sh.phase("uebertrage + loesche");
    let applied = rdir.join(format!("{ts}-execute-applied.tsv"));
    apply(sh, &mut w, &schema, &settings, &plan_path, &applied)
}

fn libraries(sh: &Shared, reader: &Connection) -> Result<HashMap<i64, String>, String> {
    let mut libs: HashMap<i64, String> = HashMap::new();
    let mut st = reader
        .prepare("SELECT id, path FROM library")
        .map_err(|e| format!("Tabelle 'library' nicht lesbar: {e} – Volume fuer /data pruefen"))?;
    let it = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
    for x in it {
        let (id, p) = x.map_err(|e| e.to_string())?;
        let mapped = map_root(&sh.env, &p);
        if !Path::new(&mapped).is_dir() {
            return Err(format!(
                "Bibliothekspfad '{mapped}' (Navidrome: '{p}') existiert in diesem Container nicht – /music-Volume bzw. PATH_MAP pruefen"
            ));
        }
        sh.log(format!("Bibliothek {id}: {p}"));
        libs.insert(id, p);
    }
    if libs.is_empty() {
        return Err("Keine Bibliothek in der Datenbank gefunden".into());
    }
    Ok(libs)
}

// ───────────────────────── Phase 1: planen ─────────────────────────

fn scan(
    sh: &Shared,
    reader: &Connection,
    schema: &Schema,
    settings: &Settings,
    libs: &HashMap<i64, String>,
    same_exts: &[String],
    plan: &mut BufWriter<fs::File>,
) -> Result<u64, String> {
    let env = &sh.env;
    let ext_sql = |v: &[String]| v.iter().map(|e| format!("'{e}'")).collect::<Vec<_>>().join(",");
    let mut union: Vec<String> = env.keep_exts.iter().chain(env.delete_exts.iter()).chain(same_exts.iter()).cloned().collect();
    union.sort();
    union.dedup();
    let all_exts = ext_sql(&union);

    let missing = if schema.mf("missing") { "missing=0 AND " } else { "" };
    sh.phase("zaehle Titel");
    let total: i64 = reader
        .query_row(&format!("SELECT count(*) FROM media_file WHERE {missing}lower(suffix) IN ({all_exts})"), [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    sh.stats.total.store(total as u64, Relaxed);
    sh.log(format!("{total} relevante Dateien ({all_exts})"));

    // Sortier-/Gruppierschluessel: identischer Ausdruck in SELECT und ORDER BY
    let ot = schema.expr("order_title", "lower(title)");
    let disc = schema.expr("disc_number", "0");
    let order = match env.match_mode {
        MatchMode::Album => format!("album_id, {disc}, {ot}"),
        MatchMode::ArtistTitle => format!("artist_id, {ot}"),
    };
    let sql = format!(
        "SELECT id, library_id, path, title, album, album_id, artist_id, {disc}, {ot}, suffix, size, duration, bit_rate, \
                {comment}, {track}, {srate}, {depth}, {chan}, {mbz}, {tags} \
         FROM media_file WHERE {missing}lower(suffix) IN ({all_exts}) ORDER BY {order}",
        comment = schema.expr("comment", "''"),
        track = schema.expr("track_number", "0"),
        srate = schema.expr("sample_rate", "0"),
        depth = schema.expr("bit_depth", "0"),
        chan = schema.expr("channels", "0"),
        mbz = schema.expr("mbz_recording_id", "''"),
        tags = if schema.mf("tags") { "CAST(tags AS TEXT)" } else { "''" },
    );

    let mut ctx = Ctx { sh, env, same_exts: same_exts.to_vec(), settings: settings.clone(), libs: libs.clone(), plan, planned: 0 };
    sh.phase("scanne und plane");

    let mut stmt = reader.prepare(&sql).map_err(|e| format!("Abfrage vorbereiten: {e}"))?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut key = String::new();
    let mut group: Vec<Row> = vec![];

    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        if sh.scan_done() {
            break;
        }
        let row = Row {
            id: s(r, 0),
            library_id: n(r, 1),
            path: s(r, 2),
            title: s(r, 3),
            album: s(r, 4),
            suffix: s(r, 9).to_lowercase(),
            size: n(r, 10),
            duration: f(r, 11),
            bit_rate: n(r, 12),
            comment: s(r, 13),
            track: n(r, 14),
            sample_rate: n(r, 15),
            bit_depth: n(r, 16),
            channels: n(r, 17),
            mbz: s(r, 18),
            tags: s(r, 19),
        };
        let (album_id, artist_id, disc_v, title_key) = (s(r, 5), s(r, 6), n(r, 7), s(r, 8));
        let k = if title_key.trim().is_empty() {
            format!("\u{0}{}", row.id) // ohne Titel nie gruppieren
        } else if env.match_mode == MatchMode::Album {
            format!("{album_id}\u{1}{disc_v}\u{1}{title_key}")
        } else {
            format!("{artist_id}\u{1}{title_key}")
        };
        sh.stats.scanned.fetch_add(1, Relaxed);
        if k != key {
            ctx.group(std::mem::take(&mut group))?;
            key = k;
        }
        group.push(row);
    }
    if !sh.scan_done() {
        ctx.group(group)?;
    }
    Ok(ctx.planned)
}

struct Ctx<'a, 'p> {
    sh: &'a Shared,
    env: &'a Env,
    same_exts: Vec<String>,
    settings: Settings,
    libs: HashMap<i64, String>,
    plan: &'p mut BufWriter<fs::File>,
    planned: u64,
}

fn map_root(env: &Env, root: &str) -> String {
    match &env.path_map {
        Some((from, to)) if root.starts_with(from.as_str()) => format!("{to}{}", &root[from.len()..]),
        _ => root.to_string(),
    }
}

fn norm(p: &str) -> String {
    p.replace('\\', "/").to_lowercase()
}

/// Dateikennung, um Hardlinks/Bind-Mounts auf dieselbe Datei zu erkennen.
#[cfg(unix)]
fn file_id(p: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(p).ok().map(|m| (m.dev(), m.ino()))
}
#[cfg(not(unix))]
fn file_id(_p: &Path) -> Option<(u64, u64)> {
    None
}

impl<'a, 'p> Ctx<'a, 'p> {
    fn full(&self, r: &Row) -> PathBuf {
        let root = self.libs.get(&r.library_id).cloned().unwrap_or_default();
        Path::new(&map_root(self.env, &root)).join(&r.path)
    }

    fn is_excluded(&self, full: &Path) -> bool {
        let hay = format!("/{}/", norm(&full.to_string_lossy()).trim_matches('/'));
        self.settings.excludes.iter().any(|e| {
            let e = norm(e.trim());
            let e = e.trim_matches('/');
            !e.is_empty() && hay.contains(&format!("/{e}/"))
        })
    }

    fn is_protected(&self, r: &Row, full: &Path) -> bool {
        let hay = format!("{}\n{}\n{}\n{}\n{}", r.title, r.album, r.comment, r.tags, full.to_string_lossy()).to_lowercase();
        self.settings.keywords.iter().any(|k| {
            let k = k.trim().to_lowercase();
            !k.is_empty() && hay.contains(&k)
        })
    }

    /// Qualitaetsrang: je groesser, desto eher wird die Datei behalten.
    fn qual(&self, r: &Row) -> (i32, i64, i64, i64, i64, i64, std::cmp::Reverse<String>) {
        let ext = match self.env.keep_exts.iter().position(|e| e == &r.suffix) {
            Some(i) => -(i as i32),
            None => -1000,
        };
        (ext, r.bit_depth, r.sample_rate, r.channels, r.bit_rate, r.size, std::cmp::Reverse(r.id.clone()))
    }

    fn write_plan(&mut self, keeper: &Row, kpath: &Path, d: &Row, dpath: &Path, reason: &str) -> Result<(), String> {
        writeln!(
            self.plan,
            "{}\t{}\t{}\t{}\t{}\t{}",
            esc(&keeper.id),
            esc(&kpath.to_string_lossy()),
            esc(&d.id),
            esc(&dpath.to_string_lossy()),
            d.size.max(0),
            esc(reason)
        )
        .map_err(|e| format!("Plan-Datei schreiben: {e}"))?;
        self.planned += 1;
        Ok(())
    }

    fn group(&mut self, g: Vec<Row>) -> Result<(), String> {
        if g.len() < 2 {
            return Ok(());
        }
        let sh = self.sh;
        let max = self.settings.max_delete;
        if max > 0 && sh.stats.duplicates.load(Relaxed) >= max {
            if !sh.limit.swap(true, Relaxed) {
                sh.log(format!("Limit von {max} Duplikaten erreicht – Scan wird sauber beendet"));
            }
            return Ok(());
        }

        // Behalten wird bevorzugt eine Datei aus KEEP_EXTS (z. B. FLAC); gibt es keine,
        // die beste Datei aus den Gleich-Format-Typen. Entfernt werden alle DELETE_EXTS-
        // Dateien (nur wenn eine KEEP-Datei existiert) und weitere Gleich-Format-Dateien.
        let keepers: Vec<&Row> = g.iter().filter(|r| self.env.keep_exts.contains(&r.suffix)).collect();
        let keeper_row: &Row = match keepers.iter().copied().max_by_key(|r| self.qual(r)) {
            Some(k) => k,
            None => match g.iter().filter(|r| self.same_exts.contains(&r.suffix)).max_by_key(|r| self.qual(r)) {
                Some(k) => k,
                None => return Ok(()),
            },
        };
        let have_keep = !keepers.is_empty();
        let dels: Vec<Row> = g
            .iter()
            .filter(|r| {
                r.id != keeper_row.id
                    && ((have_keep && self.env.delete_exts.contains(&r.suffix)) || self.same_exts.contains(&r.suffix))
            })
            .cloned()
            .collect();
        if dels.is_empty() {
            return Ok(());
        }
        sh.stats.candidates.fetch_add(1, Relaxed);

        let paths: Vec<PathBuf> = g.iter().map(|r| self.full(r)).collect();
        if paths.iter().any(|p| self.is_excluded(p)) {
            sh.stats.excluded.fetch_add(1, Relaxed);
            sh.log(format!("SKIP (Exclude-Ordner): {}", g[0].path));
            return Ok(());
        }
        if g.iter().zip(paths.iter()).any(|(r, p)| self.is_protected(r, p)) {
            sh.stats.protected.fetch_add(1, Relaxed);
            sh.log(format!("SKIP (Instrumental/Karaoke/Acapella-Schutz): {}", g[0].path));
            return Ok(());
        }

        let keeper = keeper_row.clone();
        let kpath = self.full(&keeper);
        match fs::metadata(&kpath) {
            Ok(m) if m.len() > 0 => {}
            _ => {
                sh.stats.errors.fetch_add(1, Relaxed);
                sh.log(format!("SKIP (zu behaltende Datei fehlt/leer auf Platte): {}", kpath.display()));
                return Ok(());
            }
        }
        let kid = file_id(&kpath);

        for d in dels {
            if (d.duration - keeper.duration).abs() > self.settings.duration_tol {
                sh.stats.mismatch.fetch_add(1, Relaxed);
                sh.log(format!("SKIP (Laenge weicht ab {:.1}s vs {:.1}s): {}", d.duration, keeper.duration, d.path));
                continue;
            }
            if self.settings.require_mbz_match
                && !d.mbz.trim().is_empty()
                && !keeper.mbz.trim().is_empty()
                && d.mbz.trim() != keeper.mbz.trim()
            {
                sh.stats.mismatch.fetch_add(1, Relaxed);
                sh.log(format!("SKIP (andere MusicBrainz-Recording-ID): {}", d.path));
                continue;
            }
            let dpath = self.full(&d);
            if dpath == kpath {
                continue; // dieselbe physische Datei – niemals loeschen
            }
            if kid.is_some() && kid == file_id(&dpath) {
                sh.stats.mismatch.fetch_add(1, Relaxed);
                sh.log(format!("SKIP (Hardlink auf dieselbe Datei): {}", dpath.display()));
                continue;
            }
            let same_format = d.suffix == keeper.suffix;
            if same_format && self.settings.require_same_track && d.track != keeper.track {
                sh.stats.mismatch.fetch_add(1, Relaxed);
                sh.log(format!("SKIP (Tracknummer {} vs {} bei gleichem Format): {}", d.track, keeper.track, d.path));
                continue;
            }
            let reason = if same_format {
                format!("gleiches-format:{}", d.suffix)
            } else {
                format!("{}->{}", d.suffix, keeper.suffix)
            };
            sh.stats.duplicates.fetch_add(1, Relaxed);
            if same_format {
                sh.stats.same_format.fetch_add(1, Relaxed);
            }
            if !self.sh.execute.load(Relaxed) {
                sh.stats.bytes_freed.fetch_add(d.size.max(0) as u64, Relaxed);
                sh.log(format!(
                    "[PLAN {reason}] behalte {} | loesche {} ({:.1} MB)",
                    keeper.path, d.path, d.size as f64 / 1048576.0
                ));
            }
            self.write_plan(&keeper, &kpath, &d, &dpath, &reason)?;
        }
        Ok(())
    }
}

// ── Plan-Datei: TSV mit %-Escaping, damit Tabs/Zeilenumbrueche in Pfaden nichts zerstoeren ──

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' => o.push_str("%25"),
            '\t' => o.push_str("%09"),
            '\n' => o.push_str("%0A"),
            '\r' => o.push_str("%0D"),
            _ => o.push(c),
        }
    }
    o
}
fn unesc(s: &str) -> String {
    let b = s.as_bytes();
    let mut o = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                o.push(v as char);
                i += 3;
                continue;
            }
        }
        // mehrbyte-sicher: bis zum naechsten Zeichenanfang kopieren
        let ch = s[i..].chars().next().unwrap();
        o.push(ch);
        i += ch.len_utf8();
    }
    o
}

// ───────────────────────── Phase 2: anwenden ─────────────────────────

fn apply(
    sh: &Shared,
    w: &mut Connection,
    schema: &Schema,
    settings: &Settings,
    plan_path: &Path,
    applied_path: &Path,
) -> Result<(), String> {
    let f = fs::File::open(plan_path).map_err(|e| format!("Plan-Datei lesen: {e}"))?;
    let mut applied = fs::File::create(applied_path).map(BufWriter::new).ok();
    if let Some(a) = applied.as_mut() {
        let _ = writeln!(a, "#status\tkeeper_id\tkeeper_path\tdel_id\tdel_path\tdel_size\treason");
    }
    let mut batch: Vec<PlanRow> = Vec::with_capacity(sh.env.batch_size);
    for line in BufReader::new(f).lines() {
        if sh.halt() {
            sh.log("Abbruch – restliche Plan-Eintraege bleiben unangetastet");
            break;
        }
        let line = line.map_err(|e| e.to_string())?;
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let p: Vec<&str> = line.split('\t').collect();
        if p.len() < 6 {
            sh.stats.errors.fetch_add(1, Relaxed);
            sh.log(format!("FEHLER: unlesbare Plan-Zeile: {line}"));
            continue;
        }
        batch.push(PlanRow {
            keeper_id: unesc(p[0]),
            keeper_path: unesc(p[1]),
            del_id: unesc(p[2]),
            del_path: unesc(p[3]),
            size: p[4].parse().unwrap_or(0),
            reason: unesc(p[5]),
        });
        if batch.len() >= sh.env.batch_size {
            flush(sh, w, schema, settings, std::mem::take(&mut batch), applied.as_mut())?;
        }
    }
    if !batch.is_empty() {
        flush(sh, w, schema, settings, batch, applied.as_mut())?;
    }
    if let Some(a) = applied.as_mut() {
        let _ = a.flush();
    }
    Ok(())
}

/// Ein Block = eine ACID-Transaktion. Dateien werden erst NACH dem Commit geloescht.
/// Scheitert der Block, wird er zurueckgerollt und jeder Eintrag einzeln erneut
/// versucht – ein defekter Eintrag darf einen Lauf ueber Millionen Titel nicht abbrechen.
fn flush(
    sh: &Shared,
    w: &mut Connection,
    schema: &Schema,
    settings: &Settings,
    batch: Vec<PlanRow>,
    applied: Option<&mut BufWriter<fs::File>>,
) -> Result<(), String> {
    match commit_batch(sh, w, schema, settings, &batch) {
        Ok(()) => {
            remove_files(sh, &batch, applied);
            Ok(())
        }
        Err(e) => {
            sh.log(format!("Block zurueckgerollt ({e}) – Eintraege werden einzeln geprueft"));
            for one in batch {
                let single = vec![one.clone()];
                match commit_batch(sh, w, schema, settings, &single) {
                    Ok(()) => remove_files(sh, &single, None),
                    Err(e) => {
                        sh.stats.errors.fetch_add(1, Relaxed);
                        sh.log(format!("FEHLER (uebersprungen, nichts geloescht) {}: {e}", one.del_path));
                    }
                }
            }
            Ok(())
        }
    }
}

fn commit_batch(
    sh: &Shared,
    w: &mut Connection,
    schema: &Schema,
    settings: &Settings,
    batch: &[PlanRow],
) -> Result<(), String> {
    let mut moved = 0u64;
    let mut playlists: HashSet<String> = HashSet::new();
    let tx = w.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|e| e.to_string())?;
    for p in batch {
        moved += transfer(&tx, schema, settings, &p.keeper_id, &p.del_id, &mut playlists, sh.env.delete_rows)
            .map_err(|e| format!("{}: {e}", p.del_path))?;
    }
    refresh_playlists(&tx, schema, &playlists).map_err(|e| format!("Playlist-Zaehler: {e}"))?;
    tx.commit().map_err(|e| format!("Commit fehlgeschlagen: {e}"))?;
    sh.stats.annotations.fetch_add(moved, Relaxed);
    sh.stats.playlists.fetch_add(playlists.len() as u64, Relaxed);
    Ok(())
}

fn remove_files(sh: &Shared, batch: &[PlanRow], mut applied: Option<&mut BufWriter<fs::File>>) {
    for p in batch {
        let dpath = Path::new(&p.del_path);
        let kpath = Path::new(&p.keeper_path);
        // Letzte Sicherung: die zu behaltende Datei muss existieren und nicht dieselbe sein
        if dpath == kpath {
            continue;
        }
        match fs::metadata(kpath) {
            Ok(m) if m.len() > 0 => {}
            _ => {
                sh.stats.errors.fetch_add(1, Relaxed);
                sh.log(format!(
                    "FEHLER: zu behaltende Datei fehlt kurz vor dem Loeschen, {} bleibt erhalten: {}",
                    p.del_path,
                    kpath.display()
                ));
                continue;
            }
        }
        let status = match fs::remove_file(dpath) {
            Ok(()) => "GELOESCHT",
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "FEHLTE-SCHON",
            Err(e) => {
                sh.stats.errors.fetch_add(1, Relaxed);
                sh.log(format!("FEHLER beim Loeschen {}: {e}", p.del_path));
                if let Some(a) = applied.as_mut() {
                    let _ = writeln!(a, "FEHLER\t{}\t{}\t{}\t{}\t{}\t{}", esc(&p.keeper_id), esc(&p.keeper_path), esc(&p.del_id), esc(&p.del_path), p.size, esc(&p.reason));
                }
                continue;
            }
        };
        if status == "GELOESCHT" {
            sh.stats.deleted.fetch_add(1, Relaxed);
            sh.stats.bytes_freed.fetch_add(p.size.max(0) as u64, Relaxed);
        }
        sh.log(format!("{status} {} (behalten: {})", p.del_path, p.keeper_path));
        if let Some(a) = applied.as_mut() {
            let _ = writeln!(a, "{status}\t{}\t{}\t{}\t{}\t{}\t{}", esc(&p.keeper_id), esc(&p.keeper_path), esc(&p.del_id), esc(&p.del_path), p.size, esc(&p.reason));
        }
    }
    if let Some(a) = applied.as_mut() {
        let _ = a.flush();
    }
}

// ───────────────────────── Datenuebernahme ─────────────────────────

/// Uebertraegt alles Nutzerbezogene von `del` auf `keeper` und markiert/loescht
/// danach die Zeile von `del`. Laeuft komplett in der uebergebenen Transaktion –
/// scheitert ein Schritt, bleibt die Datenbank unveraendert.
///
/// Uebernommen werden:
/// * `annotation`  – Favorit (starred/starred_at), Bewertung (rating/rated_at),
///                   Wiedergabezaehler (summiert) und letztes Abspieldatum, pro Nutzer
/// * `bookmark`    – Lesezeichen/Positionen pro Nutzer
/// * `playlist_tracks` – Playlist-Eintraege inkl. Position
/// * `scrobbles` / `scrobble_buffer` – Wiedergabe-Historie und offene Scrobbles
/// * `playqueue.items`, `share.resource_ids` – ID-Listen
/// * `media_file.average_rating` – wird fuer den Keeper neu berechnet
fn transfer(
    tx: &rusqlite::Transaction,
    sc: &Schema,
    set: &Settings,
    keeper: &str,
    del: &str,
    playlists: &mut HashSet<String>,
    delete_rows: bool,
) -> rusqlite::Result<u64> {
    if keeper == del || keeper.is_empty() || del.is_empty() {
        return Ok(0);
    }

    // ── 1. Annotationen zusammenfuehren (Favorit bleibt Favorit, Plays werden addiert) ──
    let (rc, rv, ru) = if sc.ann.contains("rated_at") {
        (
            ", rated_at",
            ", rated_at",
            ", rated_at = CASE WHEN COALESCE(excluded.rating,0) > COALESCE(rating,0) THEN excluded.rated_at ELSE rated_at END",
        )
    } else {
        ("", "", "")
    };
    let sql = format!(
        "INSERT INTO annotation (user_id, item_id, item_type, play_count, play_date, rating, starred, starred_at{rc}) \
         SELECT user_id, ?1, 'media_file', play_count, play_date, rating, starred, starred_at{rv} \
         FROM annotation WHERE item_id=?2 AND item_type='media_file' \
         ON CONFLICT(user_id, item_id, item_type) DO UPDATE SET \
           play_count = COALESCE(play_count,0) + COALESCE(excluded.play_count,0), \
           play_date = CASE WHEN excluded.play_date IS NULL THEN play_date WHEN play_date IS NULL THEN excluded.play_date ELSE max(play_date, excluded.play_date) END, \
           rating = max(COALESCE(rating,0), COALESCE(excluded.rating,0)){ru}, \
           starred = max(COALESCE(starred,0), COALESCE(excluded.starred,0)), \
           starred_at = CASE WHEN COALESCE(excluded.starred,0)=1 AND COALESCE(starred,0)=1 THEN min(COALESCE(starred_at, excluded.starred_at), COALESCE(excluded.starred_at, starred_at)) \
                             WHEN COALESCE(excluded.starred,0)=1 THEN excluded.starred_at ELSE starred_at END"
    );
    let moved = tx.execute(&sql, params![keeper, del])? as u64;

    // Verifikation: auf dem Keeper darf nichts "schlechter" sein als auf der Quelle
    let bad: i64 = tx.query_row(
        "SELECT count(*) FROM annotation a WHERE a.item_id=?2 AND a.item_type='media_file' AND NOT EXISTS ( \
           SELECT 1 FROM annotation k WHERE k.user_id=a.user_id AND k.item_id=?1 AND k.item_type='media_file' \
             AND COALESCE(k.starred,0) >= COALESCE(a.starred,0) \
             AND COALESCE(k.rating,0) >= COALESCE(a.rating,0) \
             AND COALESCE(k.play_count,0) >= COALESCE(a.play_count,0))",
        params![keeper, del],
        |r| r.get(0),
    )?;
    if bad != 0 {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "Favoriten/Bewertungen von {del} wurden nicht vollstaendig auf {keeper} uebernommen ({bad} Nutzer) – Block zurueckgerollt"
        )));
    }
    tx.execute("DELETE FROM annotation WHERE item_id=?1 AND item_type='media_file'", params![del])?;

    // ── 2. Durchschnittsbewertung des Keepers nachziehen (Navidrome haelt sie zwischengespeichert) ──
    if sc.mf("average_rating") {
        tx.execute(
            "UPDATE media_file SET average_rating = COALESCE((SELECT round(avg(rating),2) FROM annotation \
             WHERE item_id=?1 AND item_type='media_file' AND rating>0), 0) WHERE id=?1",
            params![keeper],
        )?;
    }

    // ── 3. Lesezeichen ──
    if sc.table("bookmark") && !sc.bookmark.is_empty() {
        let c = sc.bookmark.join(", ");
        tx.execute(
            &format!(
                "INSERT OR IGNORE INTO bookmark (user_id, item_id, item_type, {c}) \
                 SELECT user_id, ?1, 'media_file', {c} FROM bookmark WHERE item_id=?2 AND item_type='media_file'"
            ),
            params![keeper, del],
        )?;
        tx.execute("DELETE FROM bookmark WHERE item_id=?1 AND item_type='media_file'", params![del])?;
    }

    // ── 4. Playlists ──
    if sc.table("playlist_tracks") {
        {
            let mut st = tx.prepare_cached("SELECT DISTINCT playlist_id FROM playlist_tracks WHERE media_file_id=?1")?;
            let it = st.query_map(params![del], |r| r.get::<_, String>(0))?;
            for p in it {
                playlists.insert(p?);
            }
        }
        if set.playlist_keep_duplicates {
            // Position und Anzahl bleiben erhalten – der Titel verschwindet nie aus der Playlist,
            // selbst wenn die behaltene Datei dadurch doppelt vorkommt.
            tx.execute("UPDATE playlist_tracks SET media_file_id=?1 WHERE media_file_id=?2", params![keeper, del])?;
        } else {
            tx.execute(
                "UPDATE playlist_tracks SET media_file_id=?1 WHERE media_file_id=?2 \
                 AND playlist_id NOT IN (SELECT playlist_id FROM playlist_tracks WHERE media_file_id=?1)",
                params![keeper, del],
            )?;
            tx.execute("DELETE FROM playlist_tracks WHERE media_file_id=?1", params![del])?;
        }
    }

    // ── 5. Wiedergabe-Historie ──
    if sc.table("scrobbles") {
        tx.execute("UPDATE scrobbles SET media_file_id=?1 WHERE media_file_id=?2", params![keeper, del])?;
    }
    if sc.table("scrobble_buffer") {
        tx.execute("UPDATE OR IGNORE scrobble_buffer SET media_file_id=?1 WHERE media_file_id=?2", params![keeper, del])?;
        tx.execute("DELETE FROM scrobble_buffer WHERE media_file_id=?1", params![del])?;
    }

    // ── 6. ID-Listen (Warteschlangen, Shares) ──
    if sc.table("playqueue") {
        replace_in_list(tx, "playqueue", "items", keeper, del)?;
    }
    if sc.table("share") {
        replace_in_list(tx, "share", "resource_ids", keeper, del)?;
    }

    // ── 7. Abschlusspruefung: nichts darf mehr auf die Quelle zeigen ──
    let mut checks: Vec<String> = vec![
        "(SELECT count(*) FROM annotation WHERE item_id=?1 AND item_type='media_file')".into(),
    ];
    if sc.table("bookmark") {
        checks.push("(SELECT count(*) FROM bookmark WHERE item_id=?1 AND item_type='media_file')".into());
    }
    if sc.table("playlist_tracks") {
        checks.push("(SELECT count(*) FROM playlist_tracks WHERE media_file_id=?1)".into());
    }
    let left: i64 = tx.query_row(&format!("SELECT {}", checks.join(" + ")), params![del], |r| r.get(0))?;
    if left != 0 {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "{left} Verweise auf {del} konnten nicht umgehaengt werden – Block zurueckgerollt"
        )));
    }

    // ── 8. Quellzeile markieren oder entfernen ──
    if delete_rows {
        tx.execute("DELETE FROM media_file WHERE id=?1", params![del])?;
    } else if sc.mf("missing") {
        tx.execute("UPDATE media_file SET missing=1 WHERE id=?1", params![del])?;
    } else {
        tx.execute("DELETE FROM media_file WHERE id=?1", params![del])?;
    }
    Ok(moved)
}

/// Ersetzt eine ID in einer komma-separierten ID-Liste (`playqueue.items`, `share.resource_ids`).
/// Zweimal ausgefuehrt, damit auch direkt benachbarte Vorkommen erfasst werden.
fn replace_in_list(tx: &rusqlite::Transaction, table: &str, col: &str, keeper: &str, del: &str) -> rusqlite::Result<()> {
    if !cols(tx, table).iter().any(|c| c == col) {
        return Ok(());
    }
    let repl = format!("replace(','||{col}||',', ','||?2||',', ','||?1||',')");
    let sql = format!(
        "UPDATE {table} SET {col} = substr({repl}, 2, length({repl})-2) \
         WHERE {col} IS NOT NULL AND instr(','||{col}||',', ','||?2||',') > 0"
    );
    for _ in 0..2 {
        tx.execute(&sql, params![keeper, del])?;
    }
    Ok(())
}

/// Playlist-Kennzahlen genau so neu berechnen wie Navidrome selbst (refreshCounters).
fn refresh_playlists(tx: &rusqlite::Transaction, sc: &Schema, pls: &HashSet<String>) -> rusqlite::Result<()> {
    if pls.is_empty() || !sc.table("playlist") || !sc.table("playlist_tracks") {
        return Ok(());
    }
    let mut sets: Vec<&str> = vec![];
    if sc.playlist.contains("duration") {
        sets.push("duration = (SELECT COALESCE(SUM(m.duration),0) FROM playlist_tracks t JOIN media_file m ON m.id=t.media_file_id WHERE t.playlist_id=?1)");
    }
    if sc.playlist.contains("size") {
        sets.push("size = (SELECT COALESCE(SUM(m.size),0) FROM playlist_tracks t JOIN media_file m ON m.id=t.media_file_id WHERE t.playlist_id=?1)");
    }
    if sc.playlist.contains("song_count") {
        sets.push("song_count = (SELECT COUNT(*) FROM playlist_tracks t JOIN media_file m ON m.id=t.media_file_id WHERE t.playlist_id=?1)");
    }
    if sets.is_empty() {
        return Ok(());
    }
    let sql = format!("UPDATE playlist SET {} WHERE id=?1", sets.join(", "));
    for p in pls {
        tx.execute(&sql, params![p])?;
    }
    Ok(())
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "nd-dedupe-test-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    const SCHEMA: &str = r#"
    CREATE TABLE library (id integer primary key, path varchar not null);
    CREATE TABLE media_file (
        id varchar primary key, library_id integer not null, path varchar not null,
        title varchar, album varchar, album_id varchar, artist_id varchar,
        disc_number integer, order_title varchar, suffix varchar, size integer,
        duration real, bit_rate integer, comment varchar, track_number integer,
        sample_rate integer, bit_depth integer, channels integer,
        mbz_recording_id varchar, tags varchar, missing bool default 0,
        average_rating real not null default 0);
    CREATE TABLE annotation (
        user_id varchar not null, item_id varchar not null, item_type varchar not null,
        play_count integer default 0, play_date datetime, rating integer default 0,
        starred bool default 0 not null, starred_at datetime, rated_at datetime,
        unique (user_id, item_id, item_type));
    CREATE TABLE playlist (id varchar primary key, name varchar, duration real default 0,
        size integer default 0, song_count integer default 0);
    CREATE TABLE playlist_tracks (id integer default 0 not null, playlist_id varchar not null,
        media_file_id varchar not null);
    CREATE UNIQUE INDEX playlist_tracks_pos ON playlist_tracks (playlist_id, id);
    CREATE TABLE bookmark (user_id varchar not null, item_id varchar not null,
        item_type varchar not null, comment varchar, position integer, changed_by varchar,
        created_at datetime, updated_at datetime, unique (user_id, item_id, item_type));
    CREATE TABLE playqueue (id varchar not null, user_id varchar not null, current integer default 0,
        position real, changed_by varchar, items varchar, created_at datetime, updated_at datetime);
    CREATE TABLE scrobbles (media_file_id varchar not null, user_id varchar not null,
        submission_time integer not null);
    CREATE TABLE scrobble_buffer (user_id varchar not null, service varchar not null,
        media_file_id varchar not null, play_time datetime not null, enqueue_time datetime,
        id varchar not null default '', unique (user_id, service, media_file_id, play_time));
    CREATE TABLE share (id varchar primary key, resource_ids varchar not null, user_id varchar);
    "#;

    /// Legt eine Navidrome-aehnliche DB samt Musikdateien an.
    /// Gruppe 1: mp3 + flac (mp3 muss weichen). Gruppe 2: zwei flac (die schlechtere muss weichen).
    fn fixture() -> (Env, PathBuf, PathBuf) {
        let music = tmpdir("music");
        let cfg = tmpdir("cfg");
        for (name, bytes) in [("g1.mp3", 300usize), ("g1.flac", 900), ("g2-lo.flac", 500), ("g2-hi.flac", 1500)] {
            fs::write(music.join(name), vec![b'x'; bytes]).unwrap();
        }
        let db = cfg.join("navidrome.db");
        let c = Connection::open(&db).unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c.execute("INSERT INTO library (id, path) VALUES (1, ?1)", params![music.to_string_lossy()]).unwrap();
        let ins = "INSERT INTO media_file (id, library_id, path, title, album, album_id, artist_id, disc_number, \
                   order_title, suffix, size, duration, bit_rate, comment, track_number, sample_rate, bit_depth, \
                   channels, mbz_recording_id, tags, missing) \
                   VALUES (?1, 1, ?2, ?3, 'Album', ?4, 'art1', 1, ?5, ?6, ?7, ?8, ?9, '', ?10, 44100, ?11, 2, '', '{}', 0)";
        // Gruppe 1
        c.execute(ins, params!["mp3a", "g1.mp3", "Song One", "alb1", "song one", "mp3", 300, 180.0, 320, 1, 0]).unwrap();
        c.execute(ins, params!["flacA", "g1.flac", "Song One", "alb1", "song one", "flac", 900, 180.4, 900, 1, 16]).unwrap();
        // Gruppe 2: gleiche Datei zweimal als FLAC, einmal in besserer Qualitaet
        c.execute(ins, params!["flacLo", "g2-lo.flac", "Song Two", "alb1", "song two", "flac", 500, 200.0, 700, 2, 16]).unwrap();
        c.execute(ins, params!["flacHi", "g2-hi.flac", "Song Two", "alb1", "song two", "flac", 1500, 200.0, 1100, 2, 24]).unwrap();

        // Nutzerdaten ausschliesslich auf den Dateien, die geloescht werden
        c.execute("INSERT INTO annotation (user_id, item_id, item_type, play_count, play_date, rating, starred, starred_at) \
                   VALUES ('u1','mp3a','media_file',7,'2024-01-01 10:00:00',5,1,'2024-01-01 09:00:00')", []).unwrap();
        c.execute("INSERT INTO annotation (user_id, item_id, item_type, play_count, play_date, rating, starred, starred_at) \
                   VALUES ('u1','flacA','media_file',2,'2023-01-01 10:00:00',0,0,NULL)", []).unwrap();
        c.execute("INSERT INTO annotation (user_id, item_id, item_type, play_count, rating, starred) \
                   VALUES ('u2','mp3a','media_file',1,4,0)", []).unwrap();
        c.execute("INSERT INTO annotation (user_id, item_id, item_type, play_count, rating, starred) \
                   VALUES ('u1','flacLo','media_file',3,3,1)", []).unwrap();

        c.execute("INSERT INTO playlist (id, name) VALUES ('pl1','Favoriten')", []).unwrap();
        for (pos, mf) in [(1, "mp3a"), (2, "flacA"), (3, "flacLo")] {
            c.execute("INSERT INTO playlist_tracks (id, playlist_id, media_file_id) VALUES (?1,'pl1',?2)", params![pos, mf]).unwrap();
        }
        c.execute("UPDATE playlist SET song_count=3, duration=580.4, size=1700 WHERE id='pl1'", []).unwrap();

        c.execute("INSERT INTO bookmark (user_id,item_id,item_type,comment,position) VALUES ('u1','mp3a','media_file','spaeter',42)", []).unwrap();
        c.execute("INSERT INTO playqueue (id,user_id,current,items) VALUES ('q1','u1',0,'mp3a,flacLo,other')", []).unwrap();
        c.execute("INSERT INTO scrobbles (media_file_id,user_id,submission_time) VALUES ('mp3a','u1',111)", []).unwrap();
        c.execute("INSERT INTO scrobble_buffer (user_id,service,media_file_id,play_time) VALUES ('u1','lfm','mp3a','2024-01-01')", []).unwrap();
        c.execute("INSERT INTO share (id,resource_ids,user_id) VALUES ('s1','mp3a,flacA','u1')", []).unwrap();
        drop(c);

        let env = Env {
            db_path: db.to_string_lossy().to_string(),
            backup_dir: cfg.join("backups"),
            config_dir: cfg.clone(),
            path_map: None,
            keep_exts: vec!["flac".into()],
            delete_exts: vec!["mp3".into()],
            same_exts: vec!["flac".into()],
            duration_tol: 3.0,
            batch_size: 2,
            backup_keep: 2,
            match_mode: MatchMode::Album,
            delete_rows: false,
            ui_user: "admin".into(),
            ui_pass: "x".into(),
            generated_pass: false,
            require_dryrun: false,
            log_keep: 5,
        };
        (env, music, cfg)
    }

    #[test]
    fn escaping_roundtrip() {
        for s in ["", "normal/pfad.flac", "mit\ttab", "zeile\numbruch", "100%\tcool", "Ümläüte – ok"] {
            assert_eq!(unesc(&esc(s)), s, "roundtrip fuer {s:?}");
        }
        assert!(!esc("a\tb").contains('\t'));
    }

    #[test]
    fn dry_run_changes_nothing() {
        let (env, music, _cfg) = fixture();
        let sh = Shared::new(env);
        run(&sh, false);
        assert_eq!(sh.stats.errors.load(Relaxed), 0, "Dry-Run ohne Fehler");
        assert_eq!(sh.stats.duplicates.load(Relaxed), 2, "mp3 + schlechtere flac erkannt");
        assert_eq!(sh.stats.same_format.load(Relaxed), 1, "eine FLAC-FLAC-Dublette");
        assert_eq!(sh.stats.deleted.load(Relaxed), 0, "Dry-Run loescht nichts");
        for f in ["g1.mp3", "g1.flac", "g2-lo.flac", "g2-hi.flac"] {
            assert!(music.join(f).exists(), "{f} muss nach Dry-Run noch da sein");
        }
    }

    #[test]
    fn execute_keeps_favourites_playlists_and_history() {
        let (env, music, _cfg) = fixture();
        let db = env.db_path.clone();
        let sh = Shared::new(env);
        run(&sh, true);
        assert_eq!(sh.stats.errors.load(Relaxed), 0, "Execute ohne Fehler");
        assert_eq!(sh.stats.deleted.load(Relaxed), 2, "zwei Dateien geloescht");

        // Dateien: die jeweils beste bleibt
        assert!(!music.join("g1.mp3").exists(), "mp3 geloescht");
        assert!(music.join("g1.flac").exists(), "flac behalten");
        assert!(!music.join("g2-lo.flac").exists(), "schlechtere flac geloescht");
        assert!(music.join("g2-hi.flac").exists(), "bessere flac behalten");

        let c = Connection::open(&db).unwrap();
        let one = |sql: &str| -> i64 { c.query_row(sql, [], |r| r.get(0)).unwrap() };
        let txt = |sql: &str| -> String { c.query_row(sql, [], |r| r.get::<_, Option<String>>(0)).unwrap().unwrap_or_default() };

        // Favorit, Bewertung und Wiedergaben von u1 sind auf der FLAC gelandet
        assert_eq!(one("SELECT starred FROM annotation WHERE user_id='u1' AND item_id='flacA'"), 1, "Favorit erhalten");
        assert_eq!(one("SELECT rating FROM annotation WHERE user_id='u1' AND item_id='flacA'"), 5, "Bewertung erhalten");
        assert_eq!(one("SELECT play_count FROM annotation WHERE user_id='u1' AND item_id='flacA'"), 9, "Plays addiert (7+2)");
        assert_eq!(
            txt("SELECT play_date FROM annotation WHERE user_id='u1' AND item_id='flacA'"),
            "2024-01-01 10:00:00",
            "neueres Abspieldatum gewinnt"
        );
        // Zweiter Nutzer bekommt eine neue Zeile
        assert_eq!(one("SELECT rating FROM annotation WHERE user_id='u2' AND item_id='flacA'"), 4, "Bewertung von u2 uebernommen");
        // FLAC-FLAC: Favorit wandert auf die bessere Datei
        assert_eq!(one("SELECT starred FROM annotation WHERE user_id='u1' AND item_id='flacHi'"), 1, "Favorit der FLAC-Dublette erhalten");
        assert_eq!(one("SELECT rating FROM annotation WHERE user_id='u1' AND item_id='flacHi'"), 3);
        // Keine Reste auf den geloeschten IDs
        assert_eq!(one("SELECT count(*) FROM annotation WHERE item_id IN ('mp3a','flacLo')"), 0, "keine Annotation-Reste");
        // Durchschnittsbewertung nachgezogen
        assert_eq!(one("SELECT CAST(average_rating*100 AS INTEGER) FROM media_file WHERE id='flacA'"), 450, "avg(5,4)=4.5");

        // Playlist: gleich viele Einträge, keiner zeigt mehr auf eine geloeschte Datei
        assert_eq!(one("SELECT count(*) FROM playlist_tracks WHERE playlist_id='pl1'"), 3, "Playlist bleibt gleich lang");
        assert_eq!(one("SELECT count(*) FROM playlist_tracks WHERE media_file_id IN ('mp3a','flacLo')"), 0, "keine toten Verweise");
        assert_eq!(one("SELECT media_file_id='flacA' FROM playlist_tracks WHERE playlist_id='pl1' AND id=1"), 1, "Position 1 zeigt auf die FLAC");
        assert_eq!(one("SELECT song_count FROM playlist WHERE id='pl1'"), 3, "song_count nachgezogen");

        // Lesezeichen, Warteschlange, Historie, Share
        assert_eq!(one("SELECT position FROM bookmark WHERE user_id='u1' AND item_id='flacA'"), 42, "Lesezeichen uebernommen");
        assert_eq!(one("SELECT count(*) FROM bookmark WHERE item_id='mp3a'"), 0);
        assert_eq!(txt("SELECT items FROM playqueue WHERE id='q1'"), "flacA,flacHi,other", "Warteschlange umgeschrieben");
        assert_eq!(one("SELECT count(*) FROM scrobbles WHERE media_file_id='flacA'"), 1, "Scrobble-Historie uebernommen");
        assert_eq!(one("SELECT count(*) FROM scrobble_buffer WHERE media_file_id='flacA'"), 1);
        assert_eq!(txt("SELECT resource_ids FROM share WHERE id='s1'"), "flacA,flacA", "Share zeigt auf die FLAC");

        // Quellzeilen sind als fehlend markiert, nicht geloescht (DELETE_ROWS=false)
        assert_eq!(one("SELECT missing FROM media_file WHERE id='mp3a'"), 1, "mp3-Zeile als missing markiert");
        assert_eq!(one("SELECT count(*) FROM media_file WHERE id='flacLo' AND missing=1"), 1);

        // Backup wurde angelegt
        assert!(!list_backups(&sh.env).is_empty(), "Backup vorhanden");
    }

    #[test]
    fn protected_and_limits_are_respected() {
        let (env, music, _cfg) = fixture();
        let sh = Shared::new(env);
        {
            let mut s = sh.settings.lock().unwrap();
            s.keywords = vec!["song one".into()]; // trifft Gruppe 1 ueber den Titel
            s.max_delete = 0;
        }
        run(&sh, true);
        assert_eq!(sh.stats.protected.load(Relaxed), 1, "Gruppe 1 geschuetzt");
        assert!(music.join("g1.mp3").exists(), "geschuetzte Gruppe bleibt unangetastet");
        assert_eq!(sh.stats.deleted.load(Relaxed), 1, "nur die FLAC-Dublette geloescht");
    }

    #[test]
    fn duration_mismatch_is_skipped() {
        let (env, music, _cfg) = fixture();
        {
            let c = Connection::open(&env.db_path).unwrap();
            c.execute("UPDATE media_file SET duration=300.0 WHERE id='mp3a'", []).unwrap();
        }
        let sh = Shared::new(env);
        run(&sh, true);
        assert!(music.join("g1.mp3").exists(), "abweichende Laenge -> nicht geloescht");
        assert!(sh.stats.mismatch.load(Relaxed) >= 1);
        assert_eq!(sh.stats.errors.load(Relaxed), 0);
    }

    #[test]
    fn different_musicbrainz_ids_are_skipped() {
        let (env, music, _cfg) = fixture();
        {
            let c = Connection::open(&env.db_path).unwrap();
            c.execute("UPDATE media_file SET mbz_recording_id='aaa' WHERE id='flacLo'", []).unwrap();
            c.execute("UPDATE media_file SET mbz_recording_id='bbb' WHERE id='flacHi'", []).unwrap();
        }
        let sh = Shared::new(env);
        run(&sh, true);
        assert!(music.join("g2-lo.flac").exists(), "andere Recording-ID -> nicht geloescht");
        assert!(!music.join("g1.mp3").exists(), "Gruppe 1 bleibt davon unberuehrt");
    }

    #[test]
    fn flac_dedupe_can_be_disabled() {
        let (env, music, _cfg) = fixture();
        let sh = Shared::new(env);
        sh.settings.lock().unwrap().dedupe_flac = false;
        run(&sh, true);
        assert!(music.join("g2-lo.flac").exists(), "ohne FLAC-Bereinigung bleiben beide FLACs");
        assert!(!music.join("g1.mp3").exists(), "mp3 wird trotzdem entfernt");
        assert_eq!(sh.stats.deleted.load(Relaxed), 1);
    }

    #[test]
    fn missing_database_is_reported_and_not_created() {
        let cfg = tmpdir("nodb");
        let mut env = fixture().0;
        env.db_path = cfg.join("gibtsnicht.db").to_string_lossy().to_string();
        let path = env.db_path.clone();
        let sh = Shared::new(env);
        run(&sh, false);
        assert!(sh.stats.errors.load(Relaxed) > 0, "Fehler gemeldet");
        assert!(!Path::new(&path).exists(), "es wird keine leere DB angelegt");
    }
}
