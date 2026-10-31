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
    pub trash_dir: PathBuf,
    pub config_dir: PathBuf,
    pub path_map: Option<(String, String)>,
    pub keep_exts: Vec<String>,
    pub delete_exts: Vec<String>,
    pub same_exts: Vec<String>,
    /// Alle Formate, die der Bibliotheks-Scan indexiert (Inhaltssuche gilt fuer alle davon)
    pub audio_exts: Vec<String>,
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
    /// Wie viele Dateien gleichzeitig angefasst werden. Lohnt vor allem auf
    /// Netzwerk- und Plattenspeicher, wo jede einzelne Anfrage lange braucht.
    pub scan_threads: usize,
}

fn var(k: &str, d: &str) -> String {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| d.to_string())
}
fn list(k: &str, d: &str) -> Vec<String> {
    var(k, d)
        .split(',')
        .map(|s| s.trim().trim_start_matches('.').to_lowercase())
        .filter(|s| !s.is_empty() && s != "none" && (s == "*" || s.chars().all(|c| c.is_ascii_alphanumeric())))
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
            trash_dir: var("TRASH_DIR", "/trash").into(),
            config_dir,
            path_map,
            keep_exts: list("KEEP_EXTS", "flac"),
            delete_exts: list("DELETE_EXTS", "mp3"),
            // "*" = jedes Format, das auch sonst betrachtet wird (FLAC/FLAC, MP3/MP3, ...)
            same_exts: list("DEDUPE_SAME_EXTS", "*"),
            audio_exts: list(
                "AUDIO_EXTS",
                "flac,mp3,m4a,m4b,aac,alac,ogg,oga,opus,wma,wav,aiff,aif,ape,wv,mpc,dsf,dff",
            ),
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
            scan_threads: var("SCAN_THREADS", "0")
                .parse()
                .ok()
                .filter(|n: &usize| *n > 0)
                .unwrap_or_else(|| {
                    // Der Scan wartet fast nur auf die Platte, nicht auf die CPU –
                    // deshalb ruhig mehr Threads als Kerne, aber mit Deckel.
                    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(4, 16)
                })
                .clamp(1, 64),
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
    /// Titel wie "Song (1)", "Song (2)", "Song - Copy" werden auf "Song" normalisiert,
    /// damit solche Kopien ueberhaupt als Duplikat erkannt werden.
    pub strip_copy_suffix: bool,
    /// Haette die zu loeschende Datei Tag-Felder, die der behaltenen fehlen,
    /// wird die Gruppe uebersprungen statt die Tags zu verlieren.
    pub skip_on_tag_loss: bool,
    /// Suchverfahren: "db" (Navidrome-Gruppierung), "content" (Dateiinhalt) oder "both".
    pub mode: String,
    /// Bei nicht-FLAC-Treffern den Audiobereich zusaetzlich Byte fuer Byte vergleichen.
    pub verify_bytes: bool,
    /// Beim Bibliotheks-Scan melden, welche Dateien Navidrome nicht kennt.
    pub report_unknown: bool,
    /// Geloeschte Dateien in den Papierkorb verschieben statt entfernen.
    pub use_trash: bool,
    /// Eigene Index-Datenbank fuehren (/config/index.db).
    pub use_index: bool,
    /// Unveraenderte Dateien nicht erneut untersuchen, sondern aus dem Index nehmen.
    pub reuse_index: bool,
    /// Bei zwei Dateien im selben Album streng pruefen, ob es wirklich dasselbe
    /// Stueck ist und nicht eine andere Fassung (Live, Remix, Radio Edit ...).
    pub strict_album_match: bool,
    /// Vor dem Loeschen pruefen, ob die Datei noch die Groesse aus dem Plan hat.
    /// Weicht sie ab, wurde sie seit der Planung ersetzt – dann bleibt sie liegen.
    pub verify_size_before_delete: bool,
    /// In Playlist-Dateien (m3u), die Navidrome einliest, den Pfad mitschreiben.
    /// Sonst baut Navidrome die Playlist beim naechsten Scan aus der Datei neu auf
    /// und der Eintrag faellt heraus, weil die alte Datei nicht mehr existiert.
    pub rewrite_m3u: bool,
    /// Echter Lauf uebernimmt den Plan des letzten vollstaendigen Dry-Runs,
    /// statt die Bibliothek noch einmal zu durchsuchen.
    pub reuse_plan: bool,
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
            strip_copy_suffix: true,
            skip_on_tag_loss: false,
            mode: "both".into(),
            verify_bytes: true,
            report_unknown: true,
            use_trash: false,
            use_index: true,
            reuse_index: true,
            reuse_plan: false,
            rewrite_m3u: true,
            verify_size_before_delete: true,
            strict_album_match: true,
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
    pub album_dupes: AtomicU64,
    pub planned: AtomicU64,
    pub files: AtomicU64,
    pub reused: AtomicU64,
    pub unknown: AtomicU64,
    pub exact: AtomicU64,
    pub tags_lost: AtomicU64,
    pub protected: AtomicU64,
    pub excluded: AtomicU64,
    pub mismatch: AtomicU64,
    pub deleted: AtomicU64,
    pub bytes_freed: AtomicU64,
    pub annotations: AtomicU64,
    pub playlists: AtomicU64,
    pub m3u: AtomicU64,
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
    /// Unix-Sekunden des Laufbeginns (0 = kein Lauf) – fuer Rate und Restzeit
    pub started_ts: AtomicU64,
    /// Unix-Sekunden des Laufendes (0 = laeuft noch) – friert die Laufzeit nach dem Ende ein
    pub ended_ts: AtomicU64,
    /// zwischengespeicherte Warnungen: (Zeitstempel, Texte)
    pub warn_cache: Mutex<(i64, Vec<String>)>,
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
            started_ts: AtomicU64::new(0),
            ended_ts: AtomicU64::new(0),
            warn_cache: Mutex::new((0, vec![])),
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
            "same_format": st.same_format.load(Relaxed), "album_dupes": st.album_dupes.load(Relaxed),
            "planned": st.planned.load(Relaxed),
            "files": st.files.load(Relaxed), "reused": st.reused.load(Relaxed),
            "unknown": st.unknown.load(Relaxed),
            "exact": st.exact.load(Relaxed),
            "tags_lost": st.tags_lost.load(Relaxed),
            "protected": st.protected.load(Relaxed), "excluded": st.excluded.load(Relaxed),
            "mismatch": st.mismatch.load(Relaxed), "deleted": st.deleted.load(Relaxed),
            "bytes_freed": st.bytes_freed.load(Relaxed), "annotations": st.annotations.load(Relaxed),
            "playlists": st.playlists.load(Relaxed), "m3u": st.m3u.load(Relaxed),
            "errors": st.errors.load(Relaxed)
        })
    }
    pub fn phase(&self, p: &str) {
        *self.phase.lock().unwrap() = p.to_string();
    }
    fn reset_stats(&self) {
        let s = &self.stats;
        for a in [&s.total, &s.scanned, &s.candidates, &s.duplicates, &s.same_format, &s.album_dupes, &s.planned,
                  &s.files, &s.reused, &s.unknown, &s.exact,
                  &s.tags_lost, &s.protected, &s.excluded, &s.mismatch, &s.deleted, &s.bytes_freed,
                  &s.annotations, &s.playlists, &s.m3u, &s.errors] {
            a.store(0, Relaxed);
        }
    }
    /// Abbruch durch Nutzer oder Signal – dann wird NICHTS mehr angewendet.
    pub fn halt(&self) -> bool {
        self.stop.load(Relaxed) || self.shutdown.load(Relaxed)
    }
    /// Scan-Ende: Abbruch oder Lauf-Limit.
    pub fn scan_done(&self) -> bool {
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
        s.mode = "both".into();
        s
    })
}
pub fn save_settings(env: &Env, s: &Settings) -> std::io::Result<()> {
    fs::create_dir_all(&env.config_dir)?;
    fs::write(settings_path(env), serde_json::to_string_pretty(s).unwrap())
}

/// Sorgt dafuer, dass lange Phasen regelmaessig von sich hoeren lassen,
/// ohne das Protokoll mit einer Zeile pro Datei zu fluten.
pub struct Takt {
    last: std::time::Instant,
    every: std::time::Duration,
}

impl Takt {
    pub fn new(secs: u64) -> Takt {
        Takt { last: std::time::Instant::now(), every: std::time::Duration::from_secs(secs) }
    }
    pub fn faellig(&mut self) -> bool {
        if self.last.elapsed() >= self.every {
            self.last = std::time::Instant::now();
            return true;
        }
        false
    }
}

// ───────────────────────── Schema-Erkennung ─────────────────────────

pub fn cols(c: &Connection, table: &str) -> Vec<String> {
    let mut st = match c.prepare(&format!("PRAGMA table_info(\"{}\")", table.replace('"', ""))) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    st.query_map([], |r| r.get::<_, String>(1))
        .map(|it| it.filter_map(|x| x.ok()).collect())
        .unwrap_or_default()
}

/// Wie wird geprueft, ob ein Tag-Feld gefuellt ist?
#[derive(PartialEq, Clone, Copy)]
pub enum Kind {
    /// Text: alles ausser der leeren Zeichenkette zaehlt
    Text,
    /// Zahl: alles ausser 0 zaehlt
    Num,
    /// JSON: Navidrome legt leere Felder als '[]' oder '{}' an – die zaehlen nicht
    Json,
}

/// Tag-Felder, deren Vorhandensein pro Datei als Flag abgefragt wird.
/// (Flag, Spalte, Anzeigename, Art)
const TAG_FIELDS: &[(char, &str, &str, Kind)] = &[
    ('A', "album_artist", "Album-Interpret", Kind::Text),
    ('r', "artist", "Interpret", Kind::Text),
    ('b', "album", "Album", Kind::Text),
    ('t', "title", "Titel", Kind::Text),
    ('n', "track_number", "Tracknummer", Kind::Num),
    ('d', "disc_number", "Disc-Nummer", Kind::Num),
    ('y', "year", "Jahr", Kind::Num),
    ('D', "date", "Datum", Kind::Text),
    ('g', "genre", "Genre", Kind::Text),
    ('c', "comment", "Kommentar", Kind::Text),
    ('l', "lyrics", "Liedtext", Kind::Json),
    ('m', "mbz_recording_id", "MusicBrainz-Recording", Kind::Text),
    ('M', "mbz_album_id", "MusicBrainz-Album", Kind::Text),
    ('k', "catalog_num", "Katalognummer", Kind::Text),
    ('p', "bpm", "BPM", Kind::Num),
    ('v', "has_cover_art", "Cover", Kind::Num),
    ('s', "sort_title", "Sortiertitel", Kind::Text),
    ('x', "explicit_status", "Explicit-Kennzeichen", Kind::Text),
    ('R', "rg_track_gain", "ReplayGain", Kind::Num),
    ('T', "tags", "weitere Tags", Kind::Json),
];

pub fn tag_label(flag: char) -> &'static str {
    TAG_FIELDS.iter().find(|(f, ..)| *f == flag).map(|(_, _, l, _)| *l).unwrap_or("?")
}

/// Welche Tabellen/Spalten hat diese Navidrome-Datenbank?
pub struct Schema {
    pub mf: HashSet<String>,
    ann: HashSet<String>,
    playlist: HashSet<String>,
    /// Lesezeichen-Spalten ausser dem Schluessel (user_id, item_id, item_type)
    bookmark: Vec<String>,
    tables: HashSet<String>,
}

impl Schema {
    pub fn load(c: &Connection) -> Result<Schema, String> {
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
    pub fn mf(&self, c: &str) -> bool {
        self.mf.contains(c)
    }
    pub fn table(&self, t: &str) -> bool {
        self.tables.contains(t)
    }
    /// Spaltenausdruck fuer das SELECT: existiert die Spalte nicht, liefert `fallback`.
    pub fn expr(&self, col: &str, fallback: &str) -> String {
        if self.mf(col) { col.to_string() } else { fallback.to_string() }
    }
    /// Ein kompakter SQL-Ausdruck, der pro Zeile eine Zeichenkette mit einem Zeichen
    /// je Tag-Feld liefert ("-" = leer). So kostet die Tag-Pruefung nur ~20 Byte pro Titel,
    /// statt alle Tag-Spalten zu uebertragen.
    pub fn tag_flags_expr(&self) -> String {
        let parts: Vec<String> = TAG_FIELDS
            .iter()
            .filter(|(_, col, _, _)| self.mf(col))
            .map(|(flag, col, _, kind)| match kind {
                Kind::Num => format!("CASE WHEN COALESCE({col},0)<>0 THEN '{flag}' ELSE '-' END"),
                // Navidrome speichert leere JSON-Felder als '[]' bzw. '{{}}' – kein Inhalt
                Kind::Json => {
                    format!("CASE WHEN COALESCE({col},'') NOT IN ('','{{}}','[]','null') THEN '{flag}' ELSE '-' END")
                }
                Kind::Text => format!("CASE WHEN COALESCE({col},'')<>'' THEN '{flag}' ELSE '-' END"),
            })
            .collect();
        if parts.is_empty() { "''".to_string() } else { parts.join(" || ") }
    }
}

// ── Titel-Normalisierung: "Song (1)", "Song (2)", "Song - Copy" sind dasselbe Stueck ──

/// Entfernt einen angehaengten Zaehler in Klammern, z. B. "song (1)" -> "song".
fn strip_counter(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let close = match b.last()? {
        b')' => b'(',
        b']' => b'[',
        b'}' => b'{',
        _ => return None,
    };
    let mut i = b.len() - 1;
    let mut digits = 0usize;
    while i > 0 {
        i -= 1;
        if b[i].is_ascii_digit() {
            digits += 1;
        } else {
            break;
        }
    }
    if digits == 0 || b[i] != close {
        return None;
    }
    Some(s[..i].trim_end().to_string())
}

const COPY_SUFFIXES: &[&str] = &[
    " - copy", " copy", " - kopie", " kopie", " - copia", " copia", " - duplicate", " duplicate", " dup",
];

/// Gruppierschluessel aus einem Titel. `strip=false` gibt den Titel unveraendert (nur getrimmt) zurueck.
pub fn norm_title(t: &str, strip: bool) -> String {
    let mut s = t.trim().to_lowercase();
    if !strip {
        return s;
    }
    loop {
        let before = s.len();
        if let Some(x) = strip_counter(s.trim_end()) {
            s = x;
        }
        for suf in COPY_SUFFIXES {
            if s.ends_with(suf) {
                s.truncate(s.len() - suf.len());
            }
        }
        s = s.trim_end().trim_end_matches(['-', '_', '.']).trim_end().to_string();
        if s.len() == before {
            break;
        }
    }
    s
}

/// Woerter, die eine eigene Fassung eines Stuecks kennzeichnen. Steht eins davon
/// nur bei einer der beiden Dateien, sind es nicht dieselben Aufnahmen.
const VERSION_WORDS: &[&str] = &[
    "live", "remix", "rmx", "edit", "radio", "extended", "demo", "acoustic", "akustik", "unplugged",
    "remaster", "mono", "stereo", "version", "fassung", "alternate", "alternativ", "reprise", "bonus",
    "interlude", "cover", "session", "rehearsal", "club", "dub", "single", "original", "orchestral",
    "piano", "sped up", "slowed", "nightcore", "clean", "explicit", "edit)", "mix", "take",
];

/// Sind zwei Dateien desselben Albums wirklich dasselbe Stueck? Gibt den Grund
/// zurueck, wenn nicht. Jede einzelne Pruefung sagt: "das waere bei derselben
/// Aufnahme nicht so".
fn unterschied(a: &Row, b: &Row) -> Option<String> {
    // Der eindeutigste Fall: MusicBrainz benennt den Track auf dieser Veroeffentlichung.
    let (ra, rb) = (a.rtid.trim(), b.rtid.trim());
    if !ra.is_empty() && !rb.is_empty() && ra != rb {
        return Some("andere MusicBrainz-Release-Track-ID".into());
    }
    if a.track != b.track {
        return Some(format!("Tracknummer {} vs {}", a.track, b.track));
    }
    if a.disc != b.disc {
        return Some(format!("Disc {} vs {}", a.disc, b.disc));
    }
    // Innerhalb eines Albums darf die Laufzeit nur minimal abweichen. Eine andere
    // Fassung ist praktisch immer deutlich laenger oder kuerzer.
    if (a.duration - b.duration).abs() > 1.0 {
        return Some(format!("Laufzeit {:.1}s vs {:.1}s", a.duration, b.duration));
    }
    let (sa, sb) = (a.subtitle.trim(), b.subtitle.trim());
    if !sa.is_empty() && !sb.is_empty() && !sa.eq_ignore_ascii_case(sb) {
        return Some(format!("Disc-Untertitel '{sa}' vs '{sb}'"));
    }
    let ma = version_marks(&format!("{} {}", a.title, a.subtitle));
    let mb = version_marks(&format!("{} {}", b.title, b.subtitle));
    if ma != mb {
        return Some(format!("Fassung unterscheidet sich ({ma:?} vs {mb:?})"));
    }
    None
}

/// Welche Versionsmerkmale stecken in diesem Text?
fn version_marks(text: &str) -> Vec<&'static str> {
    let hay = text.to_lowercase();
    let mut v: Vec<&'static str> = VERSION_WORDS.iter().copied().filter(|w| hay.contains(w)).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Sieht der Dateiname nach einer Kopie aus ("track (1).flac")?
pub fn is_copy_name(path: &str) -> bool {
    let stem = Path::new(path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let stem = stem.trim().to_lowercase();
    !stem.is_empty() && norm_title(&stem, true) != stem
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
    /// MusicBrainz-Release-Track: benennt genau diesen Track auf genau dieser Veroeffentlichung
    rtid: String,
    disc: i64,
    subtitle: String,
    comment: String,
    tags: String,
    tag_flags: String,
    track: i64,
}

/// Anzahl gesetzter Tag-Felder in einer Flag-Zeichenkette.
pub fn tag_score(flags: &str) -> i64 {
    flags.chars().filter(|c| *c != '-').count() as i64
}

/// Tag-Felder, die `a` hat und `b` fehlen.
pub fn tags_missing(a: &str, b: &str) -> Vec<char> {
    let have: Vec<char> = b.chars().filter(|c| *c != '-').collect();
    a.chars().filter(|c| *c != '-' && !have.contains(c)).collect()
}

impl Row {
    /// Anzahl gesetzter Tag-Felder.
    fn tag_score(&self) -> i64 {
        tag_score(&self.tag_flags)
    }
    /// Tag-Felder, die diese Datei hat und `other` fehlen.
    fn tags_missing_in(&self, other: &Row) -> Vec<char> {
        tags_missing(&self.tag_flags, &other.tag_flags)
    }
}

/// Schreibt die Plan-Datei und verhindert, dass dieselbe Datei doppelt geplant
/// oder eine bereits zum Loeschen vorgemerkte Datei als "behalten" eingetragen wird.
pub struct Planner {
    plan: BufWriter<fs::File>,
    pub planned: u64,
    doomed: HashSet<String>,
    /// eigene Verbindung zum Index, damit der laufende Scan nicht blockiert wird
    idx: Option<(Connection, i64)>,
}

impl Planner {
    pub fn new(file: fs::File) -> Result<Planner, String> {
        let mut plan = BufWriter::new(file);
        writeln!(plan, "#keeper_id\tkeeper_path\tdel_id\tdel_path\tdel_size\treason").map_err(|e| e.to_string())?;
        Ok(Planner { plan, planned: 0, doomed: HashSet::new(), idx: None })
    }

    /// Haelt jede geplante Loeschung zusaetzlich im Index fest.
    pub fn with_index(mut self, c: Connection, run: i64) -> Planner {
        self.idx = Some((c, run));
        self
    }

    /// Merkt eine Loeschung vor. Gibt false zurueck, wenn sie verworfen wurde.
    pub fn add(
        &mut self,
        sh: &Shared,
        keeper_id: &str,
        keeper_path: &str,
        del_id: &str,
        del_path: &str,
        size: i64,
        reason: &str,
    ) -> Result<bool, String> {
        if keeper_path == del_path || del_path.is_empty() {
            return Ok(false);
        }
        if self.doomed.contains(del_path) {
            return Ok(false); // schon vorgemerkt, z. B. von einem anderen Suchverfahren
        }
        if self.doomed.contains(keeper_path) {
            // Die Datei, die wir behalten wollten, wird selbst geloescht – niemals beides.
            sh.stats.errors.fetch_add(1, Relaxed);
            sh.log(format!("SKIP (zu behaltende Datei ist selbst zum Loeschen vorgemerkt): {del_path}"));
            return Ok(false);
        }
        writeln!(
            self.plan,
            "{}\t{}\t{}\t{}\t{}\t{}",
            esc(keeper_id), esc(keeper_path), esc(del_id), esc(del_path), size.max(0), esc(reason)
        )
        .map_err(|e| format!("Plan-Datei schreiben: {e}"))?;
        if let Some((c, run)) = &self.idx {
            crate::index::add_plan(c, *run, keeper_id, keeper_path, del_id, del_path, size.max(0), reason)?;
        }
        self.doomed.insert(del_path.to_string());
        self.planned += 1;
        sh.stats.planned.fetch_add(1, Relaxed);
        Ok(true)
    }

    pub fn flush(&mut self) -> Result<(), String> {
        self.plan.flush().map_err(|e| e.to_string())
    }
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
    sh.started_ts.store(started.timestamp().max(0) as u64, Relaxed);
    sh.ended_ts.store(0, Relaxed);
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
    sh.ended_ts.store(ended.timestamp().max(0) as u64, Relaxed);
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
    // Gleiche Formate bereinigen heisst: doppelte FLACs, doppelte MP3s, doppelte M4As ...
    let same_exts: Vec<String> = if settings.dedupe_flac {
        if env.same_exts.iter().any(|e| e == "*") {
            let mut v: Vec<String> = env.keep_exts.iter().chain(env.delete_exts.iter()).cloned().collect();
            v.sort();
            v.dedup();
            v
        } else {
            env.same_exts.clone()
        }
    } else {
        vec![]
    };
    if env.delete_exts.is_empty() && same_exts.is_empty() {
        return Err("Weder DELETE_EXTS noch die Bereinigung gleicher Formate ist aktiv – es gaebe nichts zu loeschen".into());
    }

    // ── Phase 1: planen (nur lesend) ──
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let rdir = env.config_dir.join("reports");
    fs::create_dir_all(&rdir).map_err(|e| format!("Report-Ordner {}: {e}", rdir.display()))?;
    let plan_path = rdir.join(format!("{ts}-{}-plan.tsv", if execute { "execute" } else { "dryrun" }));

    let mode = settings.mode.to_lowercase();
    let (do_content, do_db) = match mode.as_str() {
        "db" => (false, true),
        "content" => (true, false),
        _ => (true, true),
    };

    let mut idx = if settings.use_index { Some(crate::index::open(env)?) } else { None };
    let run_id = match &idx {
        Some(c) => crate::index::start_run(c, &mode, execute)?,
        None => 0,
    };
    // Der echte Lauf darf den Plan des letzten vollstaendigen Dry-Runs uebernehmen.
    let mut plan_run = run_id;
    let mut reused_plan = false;
    if execute && settings.reuse_plan {
        if let Some(c) = &idx {
            if let Some((old, started, n)) = crate::index::reusable_plan(c) {
                sh.log(format!("Uebernehme den Plan des Dry-Runs vom {started} ({n} Eintraege) – es wird nicht neu gesucht"));
                let rows = crate::index::plan_rows(c, old)?;
                let mut planner = Planner::new(fs::File::create(&plan_path).map_err(|e| format!("Plan-Datei: {e}"))?)?;
                for (ki, kp, di, dp, size, reason) in rows {
                    if !Path::new(&dp).is_file() {
                        sh.log(format!("SKIP (nicht mehr vorhanden): {dp}"));
                        continue;
                    }
                    if !Path::new(&kp).is_file() {
                        sh.log(format!("SKIP (zu behaltende Datei fehlt inzwischen): {kp}"));
                        continue;
                    }
                    planner.add(sh, &ki, &kp, &di, &dp, size, &reason)?;
                }
                planner.flush()?;
                plan_run = old;
                reused_plan = true;
            } else {
                sh.log("Kein uebernehmbarer Dry-Run-Plan im Index – es wird normal gesucht");
            }
        }
    }

    let planned = if reused_plan {
        sh.stats.planned.load(Relaxed)
    } else {
        let reader = open(env, true)?;
        let schema = Schema::load(&reader)?;
        let libs = libraries(sh, &reader)?;
        sh.log(format!(
            "Schema: rated_at={} bookmark={} playqueue={} scrobbles={} scrobble_buffer={} share={} playlists={}",
            schema.ann.contains("rated_at"), schema.table("bookmark"), schema.table("playqueue"),
            schema.table("scrobbles"), schema.table("scrobble_buffer"), schema.table("share"),
            schema.table("playlist_tracks")
        ));
        sh.log(format!(
            "Suchverfahren: {}",
            match (do_content, do_db) {
                (true, true) => "Dateiinhalt + Navidrome-Datenbank",
                (true, false) => "nur Dateiinhalt",
                _ => "nur Navidrome-Datenbank",
            }
        ));
        // Fortschrittsmassstab: Navidromes Titelzahl, pro aktivem Suchverfahren einmal
        let media_count = count_relevant(&reader, &schema, &env.keep_exts, &env.delete_exts, &same_exts)?;
        let phases = do_content as u64 + do_db as u64;
        sh.stats.total.store(media_count * phases.max(1), Relaxed);
        sh.log(format!("{media_count} relevante Titel in Navidrome"));
        check_music_writable(sh, &libs);
        if settings.rewrite_m3u {
            check_m3u(sh, &reader, &schema);
        }
        if env.delete_rows {
            sh.log(
                "ACHTUNG: DELETE_ROWS=true entfernt die Navidrome-Zeilen wirklich. Dabei laufen die \
                 FTS5-Trigger des Suchindex mit, fuer die es offene Fehlerberichte gibt. Der Standard \
                 (als fehlend markieren) fasst den Suchindex nicht an und ist umkehrbar.",
            );
        }
        sh.log(format!("Report/Plan: {}", plan_path.display()));
        let planner = Planner::new(fs::File::create(&plan_path).map_err(|e| format!("Plan-Datei: {e}"))?);
        let mut planner = match (planner, &idx) {
            (Ok(p), Some(_)) => p.with_index(crate::index::open(env)?, run_id),
            (Ok(p), None) => p,
            (Err(e), _) => return Err(e),
        };
        if do_content && !sh.scan_done() {
            match idx.as_mut() {
                Some(c) => crate::content::scan(sh, env, &reader, &schema, &settings, &libs, c, run_id, &mut planner)?,
                None => {
                    // ohne Index arbeitet der Bibliotheks-Scan auf einer Wegwerf-Datenbank
                    let tmp = env.config_dir.join("scan-tmp.db");
                    let _ = fs::remove_file(&tmp);
                    let mut c = crate::index::open_at(&tmp)?;
                    let r = crate::content::scan(sh, env, &reader, &schema, &settings, &libs, &mut c, run_id, &mut planner);
                    drop(c);
                    for suffix in ["", "-wal", "-shm"] {
                        let _ = fs::remove_file(format!("{}{suffix}", tmp.display()));
                    }
                    r?
                }
            }
        }
        if do_db && !sh.scan_done() {
            scan(sh, &reader, &schema, &settings, &libs, &same_exts, &mut planner)?;
        }
        planner.flush()?;
        planner.planned
    }; // Reader hier geschlossen: ab jetzt blockiert keine Lese-Transaktion den WAL-Checkpoint

    if let Some(c) = &idx {
        crate::index::end_run(c, run_id, sh.stats.files.load(Relaxed), planned, !sh.scan_done());
    }
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
    let trash = if settings.use_trash {
        let dir = env.trash_dir.join(&ts);
        fs::create_dir_all(&dir).map_err(|e| format!("Papierkorb {}: {e}", dir.display()))?;
        sh.log(format!("Papierkorb aktiv: {} (dort liegen die entfernten Dateien)", dir.display()));
        Some(dir)
    } else {
        None
    };
    let applied = rdir.join(format!("{ts}-execute-applied.tsv"));
    apply(sh, &mut w, &schema, &settings, trash.as_deref(), idx.as_ref().map(|c| (c, plan_run)), &plan_path, &applied)
}

/// Loeschen braucht Schreibrecht auf dem **Ordner**, nicht auf der Datei.
/// Das laesst sich nur pruefen, indem man es versucht – also eine winzige
/// Datei anlegen und sofort wieder entfernen. Laeuft im Dry-Run mit, damit
/// ein schreibgeschuetzt eingebundenes Volume vorher auffaellt.
fn check_music_writable(sh: &Shared, libs: &HashMap<i64, String>) {
    for (id, root) in libs {
        let base = map_root(&sh.env, root);
        let probe = Path::new(&base).join(format!(".nd-dedupe-schreibtest-{}", std::process::id()));
        match fs::write(&probe, b"") {
            Ok(()) => {
                let _ = fs::remove_file(&probe);
            }
            Err(e) => {
                sh.log(format!(
                    "ACHTUNG: In Bibliothek {id} ({base}) laesst sich nichts anlegen oder loeschen: {e}. \
                     Die Datenbank wuerde angepasst, die Dateien blieben aber liegen – Volume read-write einbinden."
                ));
            }
        }
    }
}

/// Meldet vor dem Lauf, welche eingelesenen Playlist-Dateien angefasst werden
/// muessten und ob das ueberhaupt geht. Das laeuft auch im Dry-Run – man soll
/// nicht erst beim echten Loeschen merken, dass eine m3u schreibgeschuetzt ist.
fn check_m3u(sh: &Shared, reader: &Connection, schema: &Schema) {
    if !schema.table("playlist") || !schema.playlist.contains("path") {
        return;
    }
    let sync = if schema.playlist.contains("sync") { "COALESCE(sync,0)=1 AND " } else { "" };
    let sql = format!("SELECT name, path FROM playlist WHERE {sync}COALESCE(path,'') <> ''");
    let mut st = match reader.prepare(&sql) {
        Ok(x) => x,
        Err(_) => return,
    };
    let rows = match st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
        Ok(x) => x,
        Err(_) => return,
    };
    let (mut gesamt, mut fehlt, mut schreibgeschuetzt) = (0u32, 0u32, 0u32);
    for (name, path) in rows.flatten() {
        gesamt += 1;
        let p = map_root(&sh.env, &path);
        let f = Path::new(&p);
        if !f.is_file() {
            fehlt += 1;
            sh.log(format!("PLAYLIST-WARNUNG: Datei zu '{name}' nicht gefunden: {p}"));
            continue;
        }
        // Zum Anhaengen oeffnen aendert nichts, zeigt aber zuverlaessig,
        // ob wir schreiben duerfen (Rechte, ACLs, read-only gemountet).
        if fs::OpenOptions::new().append(true).open(f).is_err() {
            schreibgeschuetzt += 1;
            sh.log(format!("PLAYLIST-WARNUNG: '{name}' ist nicht schreibbar: {p}"));
        }
    }
    if gesamt == 0 {
        return;
    }
    if fehlt == 0 && schreibgeschuetzt == 0 {
        sh.log(format!("{gesamt} eingelesene Playlist-Datei(en) gefunden und beschreibbar"));
    } else {
        sh.log(format!(
            "ACHTUNG: von {gesamt} eingelesenen Playlist-Dateien fehlen {fehlt} und sind {schreibgeschuetzt} \
             schreibgeschuetzt. Navidrome baut diese Playlists beim naechsten Scan aus der Datei neu auf – \
             dort entfernte Titel fallen dann heraus."
        ));
    }
}

/// Zahl der Titel, die ueberhaupt in Frage kommen – dient als Massstab fuer den Fortschritt.
fn count_relevant(
    reader: &Connection,
    schema: &Schema,
    keep: &[String],
    del: &[String],
    same: &[String],
) -> Result<u64, String> {
    let mut union: Vec<String> = keep.iter().chain(del.iter()).chain(same.iter()).cloned().collect();
    union.sort();
    union.dedup();
    let list = union.iter().map(|e| format!("'{e}'")).collect::<Vec<_>>().join(",");
    let missing = if schema.mf("missing") { "missing=0 AND " } else { "" };
    let n: i64 = reader
        .query_row(&format!("SELECT count(*) FROM media_file WHERE {missing}lower(suffix) IN ({list})"), [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    Ok(n.max(0) as u64)
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
    planner: &mut Planner,
) -> Result<(), String> {
    let env = &sh.env;
    let ext_sql = |v: &[String]| v.iter().map(|e| format!("'{e}'")).collect::<Vec<_>>().join(",");
    let mut union: Vec<String> = env.keep_exts.iter().chain(env.delete_exts.iter()).chain(same_exts.iter()).cloned().collect();
    union.sort();
    union.dedup();
    let all_exts = ext_sql(&union);

    let missing = if schema.mf("missing") { "missing=0 AND " } else { "" };

    // Sortier-/Gruppierschluessel: identischer Ausdruck in SELECT und ORDER BY
    let ot = schema.expr("order_title", "lower(title)");
    let disc = schema.expr("disc_number", "0");
    let order = match env.match_mode {
        MatchMode::Album => format!("album_id, {disc}, {ot}"),
        MatchMode::ArtistTitle => format!("artist_id, {ot}"),
    };
    let sql = format!(
        "SELECT id, library_id, path, title, album, album_id, artist_id, {disc}, {ot}, suffix, size, duration, bit_rate, \
                {comment}, {track}, {srate}, {depth}, {chan}, {mbz}, {rtid}, {sub}, {flags} \
         FROM media_file WHERE {missing}lower(suffix) IN ({all_exts}) ORDER BY {order}",
        comment = schema.expr("comment", "''"),
        track = schema.expr("track_number", "0"),
        srate = schema.expr("sample_rate", "0"),
        depth = schema.expr("bit_depth", "0"),
        chan = schema.expr("channels", "0"),
        mbz = schema.expr("mbz_recording_id", "''"),
        rtid = schema.expr("mbz_release_track_id", "''"),
        sub = schema.expr("disc_subtitle", "''"),
        flags = schema.tag_flags_expr(),
    );

    let mut ctx = Ctx {
        sh,
        env,
        reader,
        has_tags: schema.mf("tags"),
        same_exts: same_exts.to_vec(),
        settings: settings.clone(),
        libs: libs.clone(),
        planner,
    };
    sh.phase("scanne und plane");

    let mut stmt = reader.prepare(&sql).map_err(|e| format!("Abfrage vorbereiten: {e}"))?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut key = String::new();
    let mut group: Vec<Row> = vec![];
    let mut takt = Takt::new(2);

    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        if sh.scan_done() {
            break;
        }
        if takt.faellig() {
            sh.log(format!(
                "… {} Titel in der Navidrome-Datenbank geprueft, {} Duplikate gefunden",
                sh.stats.scanned.load(Relaxed),
                sh.stats.duplicates.load(Relaxed)
            ));
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
            rtid: s(r, 19),
            subtitle: s(r, 20),
            disc: n(r, 7),
            tags: String::new(), // wird nur fuer Kandidaten nachgeladen
            tag_flags: s(r, 21),
        };
        let (album_id, artist_id, disc_v, raw_title) = (s(r, 5), s(r, 6), n(r, 7), s(r, 8));
        // "Song (1)" und "Song" landen damit in derselben Gruppe
        let title_key = norm_title(&raw_title, settings.strip_copy_suffix);
        let k = if title_key.is_empty() {
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
    Ok(())
}

struct Ctx<'a, 'p> {
    sh: &'a Shared,
    env: &'a Env,
    reader: &'a Connection,
    has_tags: bool,
    same_exts: Vec<String>,
    settings: Settings,
    libs: HashMap<i64, String>,
    planner: &'p mut Planner,
}

pub fn map_root(env: &Env, root: &str) -> String {
    match &env.path_map {
        Some((from, to)) if root.starts_with(from.as_str()) => format!("{to}{}", &root[from.len()..]),
        _ => root.to_string(),
    }
}

pub fn norm(p: &str) -> String {
    p.replace('\\', "/").to_lowercase()
}

/// Dateikennung, um Hardlinks/Bind-Mounts auf dieselbe Datei zu erkennen.
#[cfg(unix)]
pub fn file_id(p: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(p).ok().map(|m| (m.dev(), m.ino()))
}
#[cfg(not(unix))]
pub fn file_id(_p: &Path) -> Option<(u64, u64)> {
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
    /// Reihenfolge: Wunschformat, echte Audio-Qualitaet (Bit-Tiefe, Samplerate, Kanaele),
    /// Tag-Vollstaendigkeit, "keine Kopie"-Dateiname, dann Bitrate und Groesse.
    /// Bitrate und Groesse sind bei identischem FLAC-Audio bedeutungslos, Tags nicht –
    /// deshalb stehen die Tags davor.
    #[allow(clippy::type_complexity)]
    fn qual(&self, r: &Row) -> (i32, i64, i64, i64, i64, i64, i64, i64, std::cmp::Reverse<String>) {
        let ext = match self.env.keep_exts.iter().position(|e| e == &r.suffix) {
            Some(i) => -(i as i32),
            None => -1000,
        };
        let not_a_copy = if is_copy_name(&r.path) { 0 } else { 1 };
        (ext, r.bit_depth, r.sample_rate, r.channels, r.tag_score(), not_a_copy, r.bit_rate, r.size,
         std::cmp::Reverse(r.id.clone()))
    }

    fn write_plan(&mut self, keeper: &Row, kpath: &Path, d: &Row, dpath: &Path, reason: &str) -> Result<bool, String> {
        self.planner.add(
            self.sh,
            &keeper.id,
            &kpath.to_string_lossy(),
            &d.id,
            &dpath.to_string_lossy(),
            d.size,
            reason,
        )
    }

    fn group(&mut self, mut g: Vec<Row>) -> Result<(), String> {
        if g.len() < 2 {
            return Ok(());
        }
        // Der Tag-Block wird nur fuer die Schutz-Begriffe gebraucht. Ihn fuer jede
        // Zeile mitzulesen kostet bei Millionen Titeln hunderte Megabyte. Hier sind
        // es nur noch Gruppen mit mindestens zwei Dateien – ein Bruchteil davon.
        if self.has_tags {
            if let Ok(mut st) = self.reader.prepare_cached("SELECT CAST(tags AS TEXT) FROM media_file WHERE id=?1") {
                for r in g.iter_mut() {
                    if let Ok(t) = st.query_row(params![r.id], |x| x.get::<_, Option<String>>(0)) {
                        r.tags = t.unwrap_or_default();
                    }
                }
            }
        }
        let g = g;
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
            // Zwei Dateien im selben Album: hier muss es wirklich 1:1 dasselbe
            // Stueck sein. Eine andere Fassung – Live, Remix, Radio Edit – traegt
            // oft denselben Titel und waere sonst nicht zu unterscheiden.
            let same_album = d.album.eq_ignore_ascii_case(&keeper.album) && !d.album.trim().is_empty();
            if same_album && self.settings.strict_album_match {
                if let Some(grund) = unterschied(&keeper, &d) {
                    sh.stats.mismatch.fetch_add(1, Relaxed);
                    sh.log(format!("SKIP (im selben Album, aber {grund}): {}", d.path));
                    continue;
                }
            }
            let mut reason = if same_format {
                format!("gleiches-format:{}", d.suffix)
            } else {
                format!("{}->{}", d.suffix, keeper.suffix)
            };
            if same_album {
                reason.push_str(";album-dublette");
            }
            // Tags: haette die zu loeschende Datei Felder, die der behaltenen fehlen?
            let lost = d.tags_missing_in(&keeper);
            if !lost.is_empty() {
                let names: Vec<&str> = lost.iter().copied().map(tag_label).collect();
                reason.push_str(&format!(";tagverlust={}", lost.iter().collect::<String>()));
                sh.stats.tags_lost.fetch_add(1, Relaxed);
                if self.settings.skip_on_tag_loss {
                    sh.stats.mismatch.fetch_add(1, Relaxed);
                    sh.log(format!(
                        "SKIP (Tag-Verlust: {} nur in der Kopie vorhanden): {}",
                        names.join(", "),
                        d.path
                    ));
                    continue;
                }
                sh.log(format!(
                    "TAG-WARNUNG: {} fehlen der behaltenen Datei {} (aus {})",
                    names.join(", "),
                    keeper.path,
                    d.path
                ));
            }
            if !self.write_plan(&keeper, &kpath, &d, &dpath, &reason)? {
                continue;
            }
            sh.stats.duplicates.fetch_add(1, Relaxed);
            if same_format {
                sh.stats.same_format.fetch_add(1, Relaxed);
            }
            if same_album {
                sh.stats.album_dupes.fetch_add(1, Relaxed);
            }
            if !self.sh.execute.load(Relaxed) {
                sh.stats.bytes_freed.fetch_add(d.size.max(0) as u64, Relaxed);
                sh.log(format!(
                    "[PLAN {reason}] behalte {} | loesche {} ({:.1} MB)",
                    keeper.path, d.path, d.size as f64 / 1048576.0
                ));
            }
        }
        Ok(())
    }
}

// ── Plan-Datei: TSV mit %-Escaping, damit Tabs/Zeilenumbrueche in Pfaden nichts zerstoeren ──

pub fn esc(s: &str) -> String {
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
    trash: Option<&Path>,
    idx: Option<(&Connection, i64)>,
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
            flush(sh, w, schema, settings, trash, idx, std::mem::take(&mut batch), applied.as_mut())?;
        }
    }
    if !batch.is_empty() {
        flush(sh, w, schema, settings, trash, idx, batch, applied.as_mut())?;
    }
    if let Some(a) = applied.as_mut() {
        let _ = a.flush();
    }
    Ok(())
}

/// Verschiebt eine Datei in den Papierkorb und behaelt dabei ihren Pfad bei,
/// damit man sie spaeter zuordnen (und zurueckschieben) kann.
fn to_trash(root: &Path, file: &Path) -> std::io::Result<PathBuf> {
    let rel = file.to_string_lossy().trim_start_matches('/').replace(':', "_");
    let mut target = root.join(&rel);
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut n = 1;
    while target.exists() {
        let stem = file.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        target = root.join(&rel).with_file_name(format!("{n}-{stem}"));
        n += 1;
    }
    match fs::rename(file, &target) {
        Ok(()) => Ok(target),
        // anderes Dateisystem: kopieren und erst danach das Original entfernen
        Err(e) if e.raw_os_error() == Some(18) => {
            // Scheitert das Kopieren mittendrin (z. B. Platte voll), darf kein
            // halbes Bruchstueck im Papierkorb liegen bleiben – und das Original
            // bleibt unangetastet.
            if let Err(e) = fs::copy(file, &target) {
                let _ = fs::remove_file(&target);
                return Err(e);
            }
            fs::remove_file(file)?;
            Ok(target)
        }
        Err(e) => Err(e),
    }
}

/// Ein Block = eine ACID-Transaktion. Dateien werden erst NACH dem Commit geloescht.
/// Scheitert der Block, wird er zurueckgerollt und jeder Eintrag einzeln erneut
/// versucht – ein defekter Eintrag darf einen Lauf ueber Millionen Titel nicht abbrechen.
#[allow(clippy::too_many_arguments)]
fn flush(
    sh: &Shared,
    w: &mut Connection,
    schema: &Schema,
    settings: &Settings,
    trash: Option<&Path>,
    idx: Option<(&Connection, i64)>,
    batch: Vec<PlanRow>,
    applied: Option<&mut BufWriter<fs::File>>,
) -> Result<(), String> {
    let mut m3u: Vec<M3uFix> = vec![];
    match commit_batch(sh, w, schema, settings, &batch, &mut m3u) {
        Ok(()) => {
            rewrite_m3u(sh, &m3u);
            remove_files(sh, settings, trash, idx, &batch, applied);
            Ok(())
        }
        Err(e) => {
            sh.log(format!("Block zurueckgerollt ({e}) – Eintraege werden einzeln geprueft"));
            for one in batch {
                let single = vec![one.clone()];
                let mut m1: Vec<M3uFix> = vec![];
                match commit_batch(sh, w, schema, settings, &single, &mut m1) {
                    Ok(()) => {
                        rewrite_m3u(sh, &m1);
                        remove_files(sh, settings, trash, idx, &single, None)
                    }
                    Err(e) => {
                        sh.stats.errors.fetch_add(1, Relaxed);
                        if let Some((c, run)) = idx {
                            crate::index::set_status(c, run, &one.del_path, "fehler");
                        }
                        sh.log(format!("FEHLER (uebersprungen, nichts geloescht) {}: {e}", one.del_path));
                    }
                }
            }
            Ok(())
        }
    }
}

/// Eine Playlist-Datei, in der ein Pfad ersetzt werden muss.
pub struct M3uFix {
    pub file: String,
    pub del: String,
    pub keeper: String,
}

fn commit_batch(
    sh: &Shared,
    w: &mut Connection,
    schema: &Schema,
    settings: &Settings,
    batch: &[PlanRow],
    m3u: &mut Vec<M3uFix>,
) -> Result<(), String> {
    let mut moved = 0u64;
    let mut playlists: HashSet<String> = HashSet::new();
    let tx = w.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|e| e.to_string())?;

    for p in batch {
        // Muss VOR dem Umhaengen passieren – danach steht die alte ID nirgends mehr.
        if settings.rewrite_m3u {
            collect_m3u(&tx, schema, sh, p, m3u)?;
        }
        note_playlists(&tx, schema, &p.del_id, &mut playlists)?;
    }
    // Zustand vor der Aenderung festhalten. Nur wenn Doppelte erhalten bleiben
    // sollen, darf eine Playlist hinterher nicht kuerzer sein – beim bewussten
    // Zusammenfassen ist das Kuerzerwerden ja gerade gewollt.
    let before = if settings.playlist_keep_duplicates {
        playlist_lengths(&tx, schema, &playlists)?
    } else {
        HashMap::new()
    };

    for p in batch {
        moved += transfer(&tx, schema, settings, &p.keeper_id, &p.del_id, &mut playlists, sh.env.delete_rows)
            .map_err(|e| format!("{}: {e}", p.del_path))?;
    }
    refresh_playlists(&tx, schema, &playlists).map_err(|e| format!("Playlist-Zaehler: {e}"))?;

    // Sicherheitsgurt: war eine Playlist vorher laenger, wird der Block verworfen.
    let after = playlist_lengths(&tx, schema, &playlists)?;
    for (id, n) in &before {
        let now = after.get(id).copied().unwrap_or(0);
        if now < *n {
            return Err(format!(
                "Playlist {id} haette von {n} auf {now} Eintraege verloren – Block zurueckgerollt, nichts geloescht"
            ));
        }
    }

    tx.commit().map_err(|e| format!("Commit fehlgeschlagen: {e}"))?;
    sh.stats.annotations.fetch_add(moved, Relaxed);
    sh.stats.playlists.fetch_add(playlists.len() as u64, Relaxed);
    Ok(())
}

/// Playlists, in denen diese Datei vorkommt, fuer die Laengenpruefung vormerken.
fn note_playlists(
    tx: &rusqlite::Transaction,
    schema: &Schema,
    del_id: &str,
    out: &mut HashSet<String>,
) -> Result<(), String> {
    if !schema.table("playlist_tracks") || del_id.is_empty() {
        return Ok(());
    }
    let mut st = tx
        .prepare_cached("SELECT DISTINCT playlist_id FROM playlist_tracks WHERE media_file_id=?1")
        .map_err(|e| e.to_string())?;
    let it = st.query_map(params![del_id], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
    for p in it.flatten() {
        out.insert(p);
    }
    Ok(())
}

/// Wie viele aufloesbare Eintraege hat jede dieser Playlists gerade?
fn playlist_lengths(
    tx: &rusqlite::Transaction,
    schema: &Schema,
    ids: &HashSet<String>,
) -> Result<HashMap<String, i64>, String> {
    let mut out = HashMap::new();
    if ids.is_empty() || !schema.table("playlist_tracks") {
        return Ok(out);
    }
    let mut st = tx
        .prepare_cached(
            "SELECT count(*) FROM playlist_tracks t JOIN media_file m ON m.id = t.media_file_id WHERE t.playlist_id=?1",
        )
        .map_err(|e| e.to_string())?;
    for id in ids {
        let n: i64 = st.query_row(params![id], |r| r.get(0)).map_err(|e| e.to_string())?;
        out.insert(id.clone(), n);
    }
    Ok(out)
}

/// Sucht die von Navidrome eingelesenen Playlist-Dateien, in denen diese Datei steht.
fn collect_m3u(
    tx: &rusqlite::Transaction,
    schema: &Schema,
    sh: &Shared,
    p: &PlanRow,
    out: &mut Vec<M3uFix>,
) -> Result<(), String> {
    if !schema.table("playlist") || !schema.table("playlist_tracks") || p.del_id.is_empty() {
        return Ok(());
    }
    if !schema.playlist.contains("path") {
        return Ok(());
    }
    let sync = if schema.playlist.contains("sync") { "COALESCE(pl.sync,0)=1 AND " } else { "" };
    let sql = format!(
        "SELECT DISTINCT pl.path FROM playlist pl JOIN playlist_tracks t ON t.playlist_id = pl.id \
         WHERE t.media_file_id = ?1 AND {sync}COALESCE(pl.path,'') <> ''"
    );
    let mut st = tx.prepare_cached(&sql).map_err(|e| e.to_string())?;
    let it = st.query_map(params![p.del_id], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
    for f in it.flatten() {
        let mapped = map_root(&sh.env, &f);
        out.push(M3uFix { file: mapped, del: p.del_path.clone(), keeper: p.keeper_path.clone() });
    }
    Ok(())
}

/// Zeigen zwei Pfade auf dieselbe Datei? Zuerst der billige Textvergleich,
/// danach – nur wenn noetig – der echte, der Symlinks und `..` aufloest.
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Schreibt den neuen Pfad in die Playlist-Datei. Zeilen, die nicht passen,
/// bleiben unveraendert; geschrieben wird ueber eine Nachbardatei und `rename`,
/// damit die Playlist nie halbfertig auf der Platte liegt.
fn rewrite_m3u(sh: &Shared, fixes: &[M3uFix]) {
    let mut by_file: HashMap<&str, Vec<&M3uFix>> = HashMap::new();
    for f in fixes {
        by_file.entry(f.file.as_str()).or_default().push(f);
    }
    for (file, list) in by_file {
        let path = Path::new(file);
        let raw = match fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                sh.stats.errors.fetch_add(1, Relaxed);
                sh.log(format!("FEHLER: Playlist-Datei {file} nicht lesbar: {e}"));
                continue;
            }
        };
        // .m3u8 ist per Definition UTF-8; eine alte .m3u kann eine andere Kodierung
        // haben. Dann lieber gar nichts anfassen als die Datei zu zerschiessen.
        let text = match String::from_utf8(raw) {
            Ok(t) => t,
            Err(_) => {
                sh.stats.errors.fetch_add(1, Relaxed);
                sh.log(format!(
                    "FEHLER: Playlist-Datei {file} ist nicht UTF-8 kodiert – sie wird nicht veraendert. \
                     Bitte den Pfad dort von Hand anpassen oder die Datei als UTF-8 speichern."
                ));
                continue;
            }
        };
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let mut changed = 0usize;
        let mut out = String::with_capacity(text.len());
        for line in text.lines() {
            let t = line.trim();
            let mut replaced: Option<String> = None;
            if !t.is_empty() && !t.starts_with('#') {
                // m3u darf absolute wie relative Pfade enthalten
                let abs = if Path::new(t).is_absolute() { PathBuf::from(t) } else { dir.join(t) };
                for f in &list {
                    if same_file(&abs, Path::new(&f.del)) {
                        let keeper = Path::new(&f.keeper);
                        let rel = keeper.strip_prefix(&dir).ok();
                        replaced = Some(match (Path::new(t).is_absolute(), rel) {
                            (false, Some(r)) => r.to_string_lossy().to_string(),
                            _ => f.keeper.clone(),
                        });
                        break;
                    }
                }
            }
            match replaced {
                Some(neu) => {
                    out.push_str(&neu);
                    changed += 1;
                }
                None => out.push_str(line),
            }
            out.push('\n');
        }
        if changed == 0 {
            continue;
        }
        let tmp = path.with_extension("ndtmp");
        let ok = fs::write(&tmp, out.as_bytes()).and_then(|()| fs::rename(&tmp, path));
        match ok {
            Ok(()) => {
                sh.stats.m3u.fetch_add(1, Relaxed);
                sh.log(format!("PLAYLIST-DATEI aktualisiert ({changed} Zeile(n)): {file}"));
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                sh.stats.errors.fetch_add(1, Relaxed);
                sh.log(format!(
                    "FEHLER: Playlist-Datei {file} nicht schreibbar: {e} – Navidrome baut sie beim naechsten Scan aus der alten Datei neu auf"
                ));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn remove_files(
    sh: &Shared,
    settings: &Settings,
    trash: Option<&Path>,
    idx: Option<(&Connection, i64)>,
    batch: &[PlanRow],
    mut applied: Option<&mut BufWriter<fs::File>>,
) {
    for p in batch {
        let dpath = Path::new(&p.del_path);
        let kpath = Path::new(&p.keeper_path);
        // Letzte Sicherung: die zu behaltende Datei muss existieren und nicht dieselbe sein
        if dpath == kpath {
            continue;
        }
        // Zwischen Planung und Loeschen liegt Zeit – bei uebernommenen Plaenen
        // sogar Tage. Passt die Groesse nicht mehr, ist es nicht mehr dieselbe Datei.
        if settings.verify_size_before_delete && p.size > 0 {
            if let Ok(m) = fs::metadata(dpath) {
                if m.len() as i64 != p.size {
                    sh.stats.mismatch.fetch_add(1, Relaxed);
                    sh.log(format!(
                        "SKIP (Datei hat jetzt {} statt {} Byte – seit der Planung veraendert): {}",
                        m.len(),
                        p.size,
                        p.del_path
                    ));
                    if let Some((c, run)) = idx {
                        crate::index::set_status(c, run, &p.del_path, "veraendert");
                    }
                    continue;
                }
            }
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
        let outcome = match trash {
            Some(root) => to_trash(root, dpath).map(|_| ()),
            None => fs::remove_file(dpath),
        };
        let status = match outcome {
            Ok(()) => if trash.is_some() { "VERSCHOBEN" } else { "GELOESCHT" },
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
        if status == "GELOESCHT" || status == "VERSCHOBEN" {
            sh.stats.deleted.fetch_add(1, Relaxed);
            sh.stats.bytes_freed.fetch_add(p.size.max(0) as u64, Relaxed);
        }
        if let Some((c, run)) = idx {
            crate::index::set_status(
                c,
                run,
                &p.del_path,
                match status {
                    "GELOESCHT" => "geloescht",
                    "VERSCHOBEN" => "verschoben",
                    _ => "fehlte-schon",
                },
            );
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
    if keeper == del {
        return Ok(0);
    }
    // Kennt Navidrome die zu loeschende Datei nicht, gibt es nichts zu uebertragen.
    if del.is_empty() {
        return Ok(0);
    }
    // Umgekehrt ist es gefaehrlich: die Quelle hat Favoriten, Playlist-Plaetze und
    // Historie, aber es gibt kein Ziel dafuer. Dann lieber gar nicht loeschen.
    if keeper.is_empty() {
        return Err(rusqlite::Error::InvalidParameterName(
            "die zu behaltende Datei ist Navidrome nicht bekannt – Favoriten und Playlist-Plaetze \
             haetten kein Ziel, deshalb wird nichts geloescht"
                .into(),
        ));
    }
    // Die Zielzeile muss es auch wirklich geben (sie kann seit der Planung
    // verschwunden sein, etwa weil Navidrome dazwischen aufgeraeumt hat).
    let ziel: i64 = tx.query_row("SELECT count(*) FROM media_file WHERE id=?1", params![keeper], |r| r.get(0))?;
    if ziel == 0 {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "die zu behaltende Zeile {keeper} gibt es in Navidrome nicht mehr – nichts geloescht"
        )));
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

    /// Die aelteren Tests pruefen gezielt die Gruppierung ueber die Navidrome-Datenbank.
    fn db_mode(env: Env) -> Shared {
        let sh = Shared::new(env);
        sh.settings.lock().unwrap().mode = "db".into();
        sh
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "nd-dedupe-test-{tag}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Aus dem Schema-Dump einer echten Navidrome-Installation uebernommen:
    /// dieselben Spalten, dieselben Fremdschluessel und vor allem die FTS5-Trigger,
    /// die bei jedem INSERT, UPDATE und DELETE auf `media_file` mitlaufen.
    const SCHEMA: &str = r#"
    CREATE TABLE user (
        id varchar(255) not null primary key,
        user_name varchar(255) default '' not null unique,
        name varchar(255) collate NOCASE default '' not null,
        email varchar(255) default '' not null,
        password varchar(255) default '' not null,
        is_admin bool default FALSE not null,
        created_at datetime not null, updated_at datetime not null);
    CREATE TABLE library (
        id integer primary key autoincrement,
        name text not null unique,
        path text not null unique,
        remote_path text null default '',
        total_songs integer default 0 not null,
        total_size integer default 0 not null);
    CREATE TABLE media_file (
        id varchar(255) not null primary key,
        path varchar(255) default '' not null,
        title varchar(255) default '' not null,
        album varchar(255) default '' not null,
        artist varchar(255) default '' not null,
        artist_id varchar(255) default '' not null,
        album_artist varchar(255) default '' not null,
        album_id varchar(255) default '' not null,
        has_cover_art bool default FALSE not null,
        track_number integer default 0 not null,
        disc_number integer default 0 not null,
        year integer default 0 not null,
        size integer default 0 not null,
        suffix varchar(255) default '' not null,
        duration real default 0 not null,
        bit_rate integer default 0 not null,
        genre varchar(255) default '' not null,
        compilation bool default FALSE not null,
        created_at datetime, updated_at datetime,
        date varchar(255) default '' not null,
        sort_album_name varchar collate NOCASE default '' not null,
        sort_artist_name varchar collate NOCASE default '' not null,
        sort_album_artist_name varchar collate NOCASE default '' not null,
        sort_title varchar collate NOCASE default '' not null,
        disc_subtitle varchar default '' not null,
        catalog_num varchar default '' not null,
        comment varchar default '' not null,
        order_title varchar collate NOCASE default '' not null,
        order_album_name varchar collate NOCASE default '' not null,
        order_artist_name varchar collate NOCASE default '' not null,
        order_album_artist_name varchar collate NOCASE default '' not null,
        mbz_recording_id varchar default '' not null,
        mbz_album_id varchar default '' not null,
        mbz_release_track_id varchar default '' not null,
        channels integer default 0 not null,
        lyrics jsonb default '[]' not null,
        sample_rate integer default 0 not null,
        library_id integer default 1 not null references library on delete cascade,
        missing boolean default false not null,
        tags jsonb default '{}' not null,
        participants jsonb default '{}' not null,
        explicit_status varchar default '' not null,
        rg_track_gain real, rg_track_peak real,
        average_rating REAL NOT NULL DEFAULT 0,
        search_participants TEXT NOT NULL DEFAULT '',
        search_normalized TEXT NOT NULL DEFAULT '',
        codec VARCHAR(255) DEFAULT '' NOT NULL,
        bpm integer, bit_depth integer);
    CREATE INDEX media_file_path ON media_file (path);
    CREATE TABLE annotation (
        user_id varchar(255) not null REFERENCES user(id) ON DELETE CASCADE ON UPDATE CASCADE,
        item_id varchar(255) default '' not null,
        item_type varchar(255) default '' not null,
        play_count integer default 0,
        play_date datetime,
        rating integer default 0,
        starred bool default FALSE not null,
        starred_at datetime, rated_at datetime,
        unique (user_id, item_id, item_type));
    CREATE TABLE playlist (
        id varchar(255) not null primary key,
        name varchar(255) collate NOCASE default '' not null,
        comment varchar(255) default '' not null,
        duration real default 0 not null,
        song_count integer default 0 not null,
        public bool default FALSE not null,
        created_at datetime, updated_at datetime,
        path string default '' not null,
        sync bool default false not null,
        size integer default 0 not null,
        rules varchar, evaluated_at datetime,
        owner_id varchar(255) not null references user on update cascade on delete cascade,
        average_rating REAL NOT NULL DEFAULT 0,
        imported_hash varchar default '' not null);
    CREATE TABLE playlist_tracks (
        id integer default 0 not null,
        playlist_id varchar(255) not null references playlist on update cascade on delete cascade,
        media_file_id varchar(255) not null);
    CREATE UNIQUE INDEX playlist_tracks_pos ON playlist_tracks (playlist_id, id);
    CREATE TABLE bookmark (
        user_id varchar(255) not null references user on update cascade on delete cascade,
        item_id varchar(255) not null,
        item_type varchar(255) not null,
        comment varchar(255), position integer, changed_by varchar(255),
        created_at datetime, updated_at datetime,
        constraint bookmark_pk unique (user_id, item_id, item_type));
    CREATE TABLE playqueue (
        id varchar(255) not null,
        user_id varchar(255) not null references user(id) on update cascade on delete cascade,
        current integer not null default 0,
        changed_by varchar(255), items varchar(255),
        created_at datetime, updated_at datetime, position integer);
    CREATE TABLE scrobbles (
        id INTEGER PRIMARY KEY,
        media_file_id VARCHAR(255) NOT NULL REFERENCES media_file(id) ON DELETE CASCADE ON UPDATE CASCADE,
        user_id VARCHAR(255) NOT NULL REFERENCES user(id) ON DELETE CASCADE ON UPDATE CASCADE,
        submission_time INTEGER NOT NULL);
    CREATE TABLE scrobble_buffer (
        user_id varchar NOT NULL REFERENCES user ON UPDATE CASCADE ON DELETE CASCADE,
        service varchar NOT NULL,
        media_file_id varchar NOT NULL REFERENCES media_file ON UPDATE CASCADE ON DELETE CASCADE,
        play_time datetime NOT NULL,
        enqueue_time datetime NOT NULL DEFAULT CURRENT_TIMESTAMP,
        id varchar NOT NULL DEFAULT '',
        CONSTRAINT scrobble_buffer_pk UNIQUE (user_id, service, media_file_id, play_time));
    CREATE TABLE share (
        id varchar(255) not null primary key,
        expires_at datetime, last_visited_at datetime,
        resource_ids varchar not null,
        created_at datetime, updated_at datetime,
        user_id varchar(255) not null references user on update cascade on delete cascade,
        downloadable bool not null default false,
        description varchar not null default '',
        resource_type varchar not null default '',
        contents varchar not null default '',
        format varchar not null default '',
        max_bit_rate integer not null default 0,
        visit_count integer not null default 0);
    CREATE TABLE media_file_tags (
        media_file_id varchar not null references media_file (id) on delete cascade,
        tag_id varchar not null,
        constraint media_file_tags unique (media_file_id, tag_id));

    CREATE VIRTUAL TABLE media_file_fts USING fts5(
        title, album, artist, album_artist,
        sort_title, sort_album_name, sort_artist_name, sort_album_artist_name,
        disc_subtitle, search_participants, search_normalized,
        content='', content_rowid='rowid', tokenize='unicode61 remove_diacritics 2');
    CREATE TRIGGER media_file_fts_ai AFTER INSERT ON media_file BEGIN
        INSERT INTO media_file_fts(rowid, title, album, artist, album_artist,
            sort_title, sort_album_name, sort_artist_name, sort_album_artist_name,
            disc_subtitle, search_participants, search_normalized)
        VALUES (NEW.rowid, NEW.title, NEW.album, NEW.artist, NEW.album_artist,
            NEW.sort_title, NEW.sort_album_name, NEW.sort_artist_name, NEW.sort_album_artist_name,
            COALESCE(NEW.disc_subtitle, ''), COALESCE(NEW.search_participants, ''),
            COALESCE(NEW.search_normalized, ''));
    END;
    CREATE TRIGGER media_file_fts_ad AFTER DELETE ON media_file BEGIN
        INSERT INTO media_file_fts(media_file_fts, rowid, title, album, artist, album_artist,
            sort_title, sort_album_name, sort_artist_name, sort_album_artist_name,
            disc_subtitle, search_participants, search_normalized)
        VALUES ('delete', OLD.rowid, OLD.title, OLD.album, OLD.artist, OLD.album_artist,
            OLD.sort_title, OLD.sort_album_name, OLD.sort_artist_name, OLD.sort_album_artist_name,
            COALESCE(OLD.disc_subtitle, ''), COALESCE(OLD.search_participants, ''),
            COALESCE(OLD.search_normalized, ''));
    END;
    CREATE TRIGGER media_file_fts_au AFTER UPDATE ON media_file
    WHEN OLD.title IS NOT NEW.title OR OLD.album IS NOT NEW.album OR OLD.artist IS NOT NEW.artist
      OR OLD.album_artist IS NOT NEW.album_artist OR OLD.sort_title IS NOT NEW.sort_title
      OR OLD.sort_album_name IS NOT NEW.sort_album_name OR OLD.sort_artist_name IS NOT NEW.sort_artist_name
      OR OLD.sort_album_artist_name IS NOT NEW.sort_album_artist_name
      OR OLD.disc_subtitle IS NOT NEW.disc_subtitle
      OR OLD.search_participants IS NOT NEW.search_participants
      OR OLD.search_normalized IS NOT NEW.search_normalized
    BEGIN
        INSERT INTO media_file_fts(media_file_fts, rowid, title, album, artist, album_artist,
            sort_title, sort_album_name, sort_artist_name, sort_album_artist_name,
            disc_subtitle, search_participants, search_normalized)
        VALUES ('delete', OLD.rowid, OLD.title, OLD.album, OLD.artist, OLD.album_artist,
            OLD.sort_title, OLD.sort_album_name, OLD.sort_artist_name, OLD.sort_album_artist_name,
            COALESCE(OLD.disc_subtitle, ''), COALESCE(OLD.search_participants, ''),
            COALESCE(OLD.search_normalized, ''));
        INSERT INTO media_file_fts(rowid, title, album, artist, album_artist,
            sort_title, sort_album_name, sort_artist_name, sort_album_artist_name,
            disc_subtitle, search_participants, search_normalized)
        VALUES (NEW.rowid, NEW.title, NEW.album, NEW.artist, NEW.album_artist,
            NEW.sort_title, NEW.sort_album_name, NEW.sort_artist_name, NEW.sort_album_artist_name,
            COALESCE(NEW.disc_subtitle, ''), COALESCE(NEW.search_participants, ''),
            COALESCE(NEW.search_normalized, ''));
    END;
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
        c.execute("INSERT INTO library (id, name, path) VALUES (1, 'Musik', ?1)", params![music.to_string_lossy()]).unwrap();
        for u in ["u1", "u2"] {
            c.execute(
                "INSERT INTO user (id, user_name, created_at, updated_at) VALUES (?1, ?1, datetime('now'), datetime('now'))",
                params![u],
            )
            .unwrap();
        }
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

        c.execute("INSERT INTO playlist (id, name, owner_id) VALUES ('pl1','Favoriten','u1')", []).unwrap();
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
            trash_dir: cfg.join("trash"),
            config_dir: cfg.clone(),
            path_map: None,
            keep_exts: vec!["flac".into()],
            delete_exts: vec!["mp3".into()],
            same_exts: vec!["flac".into(), "mp3".into()],
            audio_exts: vec!["flac".into(), "mp3".into()],
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
            scan_threads: 2,
        };
        (env, music, cfg)
    }

    /// Haengt ein Paar "g3.flac" / "g3 (1).flac" an die Fixture an.
    /// Das Original hat Genre und Liedtext, die Kopie nur einen Kommentar –
    /// so laesst sich Kopie-Erkennung und Tag-Warnung zusammen pruefen.
    fn add_copy_pair(env: &Env, music: &Path) {
        fs::write(music.join("g3.flac"), vec![b'x'; 800]).unwrap();
        fs::write(music.join("g3 (1).flac"), vec![b'x'; 800]).unwrap();
        let c = Connection::open(&env.db_path).unwrap();
        let ins = "INSERT INTO media_file (id, library_id, path, title, album, album_id, artist_id, disc_number, \
                   order_title, suffix, size, duration, bit_rate, comment, track_number, sample_rate, bit_depth, \
                   channels, mbz_recording_id, tags, missing, genre, lyrics) \
                   VALUES (?1, 1, ?2, ?3, 'Album', 'alb1', 'art1', 1, ?4, 'flac', 800, 240.0, 800, ?5, 3, 44100, 16, 2, '', '{}', 0, ?6, ?7)";
        c.execute(ins, params!["flacOrig", "g3.flac", "Song Three", "song three", "", "Rock", "[{\"value\":\"la la\"}]"]).unwrap();
        c.execute(ins, params!["flacCopy", "g3 (1).flac", "Song Three (1)", "song three (1)", "aus dem Netz", "", "[]"]).unwrap();
    }

    #[test]
    fn title_normalisation() {
        for (raw, want) in [
            ("Song (1)", "song"),
            ("Song (12)", "song"),
            ("Song [2]", "song"),
            ("Song - Copy", "song"),
            ("Song Kopie", "song"),
            ("Song (1) (2)", "song"),
            ("Song", "song"),
            // echte Klammer-Zusaetze duerfen nicht verschwinden
            ("Song (Live)", "song (live)"),
            ("Song (Remix 2)", "song (remix 2)"),
            ("Nocturne No. 2", "nocturne no. 2"),
        ] {
            assert_eq!(norm_title(raw, true), want, "norm_title({raw:?})");
        }
        // abgeschaltet bleibt der Titel unveraendert
        assert_eq!(norm_title("Song (1)", false), "song (1)");
        assert!(is_copy_name("/musik/track (1).flac"));
        assert!(is_copy_name("/musik/track - Copy.flac"));
        assert!(!is_copy_name("/musik/track.flac"));
        assert!(!is_copy_name("/musik/track (Live).flac"));
    }

    #[test]
    fn copy_suffix_duplicates_are_found_and_original_kept() {
        let (env, music, _cfg) = fixture();
        add_copy_pair(&env, &music);
        let sh = db_mode(env);
        run(&sh, true);
        assert_eq!(sh.stats.errors.load(Relaxed), 0);
        assert!(music.join("g3.flac").exists(), "Original mit den besseren Tags bleibt");
        assert!(!music.join("g3 (1).flac").exists(), "die (1)-Kopie wird geloescht");
        assert_eq!(sh.stats.tags_lost.load(Relaxed), 1, "Kommentar nur in der Kopie -> Warnung");
    }

    #[test]
    fn copy_suffix_detection_can_be_disabled() {
        let (env, music, _cfg) = fixture();
        add_copy_pair(&env, &music);
        let sh = db_mode(env);
        sh.settings.lock().unwrap().strip_copy_suffix = false;
        run(&sh, true);
        assert!(music.join("g3 (1).flac").exists(), "ohne Normalisierung bleibt die Kopie liegen");
        assert_eq!(sh.stats.tags_lost.load(Relaxed), 0);
    }

    #[test]
    fn tag_loss_can_block_the_deletion() {
        let (env, music, _cfg) = fixture();
        add_copy_pair(&env, &music);
        let sh = db_mode(env);
        sh.settings.lock().unwrap().skip_on_tag_loss = true;
        run(&sh, true);
        assert!(music.join("g3 (1).flac").exists(), "Tag-Verlust verhindert das Loeschen");
        assert_eq!(sh.stats.tags_lost.load(Relaxed), 1);
        assert!(!music.join("g1.mp3").exists(), "andere Gruppen laufen normal weiter");
    }

    #[test]
    fn better_tags_win_when_audio_quality_is_equal() {
        let (env, music, _cfg) = fixture();
        add_copy_pair(&env, &music);
        // Die Kopie ist groesser und hat die hoehere Bitrate - trotzdem muss das
        // besser getaggte Original gewinnen, weil Tags vor Bitrate/Groesse zaehlen.
        // Datei und Datenbank muessen dabei zusammenpassen, sonst greift die
        // Groessenpruefung vor dem Loeschen.
        {
            fs::write(music.join("g3 (1).flac"), vec![b'x'; 99999]).unwrap();
            let c = Connection::open(&env.db_path).unwrap();
            c.execute("UPDATE media_file SET size=99999, bit_rate=1200 WHERE id='flacCopy'", []).unwrap();
        }
        let sh = db_mode(env);
        run(&sh, true);
        assert!(music.join("g3.flac").exists(), "besser getaggtes Original bleibt");
        assert!(!music.join("g3 (1).flac").exists());
    }

    #[test]
    fn audio_quality_still_beats_tags() {
        let (env, music, _cfg) = fixture();
        add_copy_pair(&env, &music);
        // Die Kopie ist 24 bit - echte Audio-Qualitaet schlaegt die Tags,
        // der Tag-Verlust wird aber protokolliert.
        {
            let c = Connection::open(&env.db_path).unwrap();
            c.execute("UPDATE media_file SET bit_depth=24 WHERE id='flacCopy'", []).unwrap();
        }
        let sh = db_mode(env);
        run(&sh, true);
        assert!(music.join("g3 (1).flac").exists(), "24-bit-Datei bleibt");
        assert!(!music.join("g3.flac").exists());
        assert!(sh.stats.tags_lost.load(Relaxed) >= 1, "Genre/Liedtext-Verlust gemeldet");
    }

    // ───────── Inhaltsbasierte Suche (vollstaendiger Bibliotheks-Scan) ─────────

    fn content_mode(env: Env) -> Shared {
        let sh = Shared::new(env);
        let mut st = sh.settings.lock().unwrap();
        st.mode = "content".into();
        drop(st);
        sh
    }

    /// Legt zwei FLAC-Dateien mit identischem Audio an, die Navidrome niemals
    /// zusammen gruppieren wuerde: anderes Album, anderer Titel, anderer Ordner.
    fn content_fixture() -> (Env, PathBuf, PathBuf) {
        let (env, music, cfg) = fixture();
        fs::create_dir_all(music.join("Alben/Nachtflug")).unwrap();
        fs::create_dir_all(music.join("Downloads")).unwrap();
        let md5 = [0x7au8; 16];
        fs::write(music.join("Alben/Nachtflug/01 Lied.flac"), crate::audio::flac_bytes(md5, 44100, 16, 2, 441000, 400)).unwrap();
        fs::write(music.join("Downloads/irgendwas.flac"), crate::audio::flac_bytes(md5, 44100, 16, 2, 441000, 900)).unwrap();

        let c = Connection::open(&env.db_path).unwrap();
        let ins = "INSERT INTO media_file (id, library_id, path, title, album, album_id, artist_id, disc_number, \
                   order_title, suffix, size, duration, bit_rate, comment, track_number, sample_rate, bit_depth, \
                   channels, mbz_recording_id, tags, missing, genre, lyrics) \
                   VALUES (?1, 1, ?2, ?3, ?4, ?5, 'art1', 1, ?6, 'flac', ?7, 10.0, 900, '', 1, 44100, 16, 2, '', '{}', 0, ?8, ?9)";
        c.execute(ins, params!["album1", "Alben/Nachtflug/01 Lied.flac", "Lied", "Nachtflug", "albN", "lied", 450, "Rock", "[{\"value\":\"text\"}]"]).unwrap();
        c.execute(ins, params!["dl1", "Downloads/irgendwas.flac", "irgendwas", "Unbekannt", "albU", "irgendwas", 950, "", "[]"]).unwrap();
        // Favorit und Playlist-Eintrag haengen an der Datei, die weichen soll
        c.execute("INSERT INTO annotation (user_id,item_id,item_type,play_count,rating,starred,starred_at) \
                   VALUES ('u1','dl1','media_file',11,4,1,'2025-01-01')", []).unwrap();
        c.execute("INSERT INTO playlist_tracks (id, playlist_id, media_file_id) VALUES (9,'pl1','dl1')", []).unwrap();
        (env, music, cfg)
    }

    #[test]
    fn inhaltssuche_findet_identische_flacs_ueber_alben_hinweg() {
        let (env, music, _cfg) = content_fixture();
        let db = env.db_path.clone();
        let sh = content_mode(env);
        run(&sh, true);

        assert_eq!(sh.stats.errors.load(Relaxed), 0, "ohne Fehler");
        assert_eq!(sh.stats.exact.load(Relaxed), 1, "ueber die FLAC-Pruefsumme erkannt");
        assert!(music.join("Alben/Nachtflug/01 Lied.flac").exists(), "besser getaggte Datei bleibt");
        assert!(!music.join("Downloads/irgendwas.flac").exists(), "die Kopie im Download-Ordner geht");

        let c = Connection::open(&db).unwrap();
        let one = |sql: &str| -> i64 { c.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(one("SELECT starred FROM annotation WHERE user_id='u1' AND item_id='album1'"), 1,
                   "Favorit wandert auf die behaltene Datei");
        assert_eq!(one("SELECT play_count FROM annotation WHERE user_id='u1' AND item_id='album1'"), 11);
        assert_eq!(one("SELECT count(*) FROM playlist_tracks WHERE media_file_id='dl1'"), 0, "keine toten Playlist-Verweise");
        assert_eq!(one("SELECT count(*) FROM playlist_tracks WHERE media_file_id='album1'"), 1,
                   "Playlist zeigt jetzt auf die behaltene Datei");
    }

    #[test]
    fn datenbankmodus_findet_diese_faelle_nicht() {
        // Gegenprobe: ueber Album und Titel gruppiert, bleiben beide Dateien liegen.
        let (env, music, _cfg) = content_fixture();
        let sh = db_mode(env);
        run(&sh, true);
        assert!(music.join("Downloads/irgendwas.flac").exists(),
                "anderes Album, anderer Titel – die Datenbanksuche kann das nicht sehen");
    }

    #[test]
    fn inhaltssuche_erkennt_dateien_ohne_navidrome_eintrag() {
        let (env, music, _cfg) = content_fixture();
        // dieselbe Musik noch einmal, aber Navidrome kennt die Datei nicht
        fs::write(music.join("Downloads/unbekannt.flac"), crate::audio::flac_bytes([0x7a; 16], 44100, 16, 2, 441000, 50)).unwrap();
        let sh = content_mode(env);
        run(&sh, true);
        assert!(sh.stats.unknown.load(Relaxed) >= 1, "nicht indizierte Datei gemeldet");
        assert!(!music.join("Downloads/unbekannt.flac").exists(), "unbekannte Kopie wird entfernt");
        assert!(music.join("Alben/Nachtflug/01 Lied.flac").exists(), "die Navidrome bekannte Datei bleibt");
    }

    #[test]
    fn papierkorb_verschiebt_statt_zu_loeschen() {
        let (env, music, _cfg) = content_fixture();
        let trash = env.trash_dir.clone();
        let sh = content_mode(env);
        sh.settings.lock().unwrap().use_trash = true;
        run(&sh, true);
        assert!(!music.join("Downloads/irgendwas.flac").exists(), "aus der Bibliothek verschwunden");
        let found = walk_find(&trash, "irgendwas.flac");
        assert!(found, "liegt im Papierkorb unter {}", trash.display());
        assert_eq!(sh.stats.deleted.load(Relaxed), 1);
    }

    fn walk_find(dir: &Path, name: &str) -> bool {
        let rd = match fs::read_dir(dir) { Ok(x) => x, Err(_) => return false };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if walk_find(&p, name) { return true; }
            } else if p.file_name().map(|n| n == name).unwrap_or(false) {
                return true;
            }
        }
        false
    }

    #[test]
    fn bytevergleich_verhindert_falsche_treffer() {
        let (env, music, _cfg) = fixture();
        fs::create_dir_all(music.join("Rest")).unwrap();
        // gleiche Laenge, aber ein Byte unterschiedlich – darf nichts loeschen
        fs::write(music.join("Rest/a.mp3"), vec![b'M'; 4000]).unwrap();
        let mut other = vec![b'M'; 4000];
        other[3000] = b'Z';
        fs::write(music.join("Rest/b.mp3"), other).unwrap();
        let sh = content_mode(env);
        run(&sh, true);
        assert!(music.join("Rest/a.mp3").exists());
        assert!(music.join("Rest/b.mp3").exists(), "unterschiedliches Audio bleibt unangetastet");
    }

    #[test]
    fn identische_mp3s_mit_anderen_tags_werden_erkannt() {
        let (env, music, _cfg) = fixture();
        fs::create_dir_all(music.join("Rest")).unwrap();
        let audio = vec![b'M'; 4000];
        let mut a = b"ID3\x04\x00\x00\x00\x00\x00\x14".to_vec();
        a.extend(std::iter::repeat(b'X').take(20));
        a.extend_from_slice(&audio);
        let mut b = b"ID3\x04\x00\x00\x00\x00\x00\x28".to_vec();
        b.extend(std::iter::repeat(b'Y').take(40));
        b.extend_from_slice(&audio);
        fs::write(music.join("Rest/mit-tags.mp3"), &a).unwrap();
        fs::write(music.join("Rest/mehr-tags.mp3"), &b).unwrap();
        let sh = content_mode(env);
        run(&sh, true);
        assert_eq!(sh.stats.errors.load(Relaxed), 0);
        let left = ["Rest/mit-tags.mp3", "Rest/mehr-tags.mp3"].iter().filter(|p| music.join(p).exists()).count();
        assert_eq!(left, 1, "von zwei inhaltsgleichen MP3s bleibt genau eine");
    }

    // ───────── Index-Datenbank ─────────

    #[test]
    fn doppelte_mp3s_ohne_flac_werden_bereinigt() {
        let (env, music, _cfg) = fixture();
        fs::write(music.join("solo-192.mp3"), vec![b'y'; 400]).unwrap();
        fs::write(music.join("solo-320.mp3"), vec![b'z'; 900]).unwrap();
        {
            let c = Connection::open(&env.db_path).unwrap();
            let ins = "INSERT INTO media_file (id, library_id, path, title, album, album_id, artist_id, disc_number, \
                       order_title, suffix, size, duration, bit_rate, comment, track_number, sample_rate, bit_depth, \
                       channels, mbz_recording_id, tags, missing, genre) \
                       VALUES (?1, 1, ?2, 'Solo', 'Album', 'albS', 'art1', 1, 'solo', 'mp3', ?3, 150.0, ?4, '', 1, 44100, 0, 2, '', '{}', 0, ?5)";
            c.execute(ins, params!["mp3lo", "solo-192.mp3", 400, 192, ""]).unwrap();
            c.execute(ins, params!["mp3hi", "solo-320.mp3", 900, 320, "Pop"]).unwrap();
            c.execute("INSERT INTO annotation (user_id,item_id,item_type,play_count,rating,starred) \
                       VALUES ('u1','mp3lo','media_file',4,5,1)", []).unwrap();
        }
        let sh = db_mode(env);
        let db = sh.env.db_path.clone();
        run(&sh, true);
        assert_eq!(sh.stats.errors.load(Relaxed), 0);
        assert!(music.join("solo-320.mp3").exists(), "die bessere MP3 bleibt");
        assert!(!music.join("solo-192.mp3").exists(), "die schlechtere MP3 geht, auch ohne FLAC");
        let c = Connection::open(&db).unwrap();
        let starred: i64 = c
            .query_row("SELECT starred FROM annotation WHERE user_id='u1' AND item_id='mp3hi'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(starred, 1, "Favorit wandert auf die behaltene MP3");
    }

    #[test]
    fn index_wird_angelegt_und_beim_zweiten_lauf_genutzt() {
        let (env, _music, cfg) = content_fixture();
        let sh = content_mode(env);
        run(&sh, false);
        let erster = sh.stats.files.load(Relaxed);
        assert!(erster > 0, "Dateien im Index erfasst");
        assert_eq!(sh.stats.reused.load(Relaxed), 0, "beim ersten Lauf gibt es nichts zu uebernehmen");
        assert!(cfg.join("index.db").is_file(), "Index-Datenbank angelegt");

        run(&sh, false);
        assert_eq!(sh.stats.files.load(Relaxed), erster, "gleiche Dateizahl");
        assert_eq!(sh.stats.reused.load(Relaxed), erster, "zweiter Lauf nimmt alles aus dem Index");
    }

    #[test]
    fn geaenderte_dateien_werden_neu_eingelesen() {
        let (env, music, _cfg) = content_fixture();
        let sh = content_mode(env);
        run(&sh, false);
        let n = sh.stats.files.load(Relaxed);
        // eine Datei austauschen: andere Groesse, andere Zeit
        fs::write(music.join("Downloads/irgendwas.flac"), crate::audio::flac_bytes([0x99; 16], 44100, 16, 2, 441000, 1234)).unwrap();
        run(&sh, false);
        assert_eq!(sh.stats.files.load(Relaxed), n);
        assert_eq!(sh.stats.reused.load(Relaxed), n - 1, "genau die geaenderte Datei wird neu gelesen");
    }

    #[test]
    fn plan_aus_dem_dry_run_wird_uebernommen() {
        let (env, music, _cfg) = content_fixture();
        let sh = content_mode(env);
        run(&sh, false);
        assert_eq!(sh.stats.planned.load(Relaxed), 1, "ein Duplikat vorgemerkt");

        sh.settings.lock().unwrap().reuse_plan = true;
        run(&sh, true);
        assert_eq!(sh.stats.files.load(Relaxed), 0, "der echte Lauf durchsucht nichts mehr");
        assert_eq!(sh.stats.deleted.load(Relaxed), 1, "er wendet den gespeicherten Plan an");
        assert!(!music.join("Downloads/irgendwas.flac").exists());
        assert!(music.join("Alben/Nachtflug/01 Lied.flac").exists());
    }

    #[test]
    fn index_laesst_sich_durchsuchen_und_filtern() {
        let (env, _music, _cfg) = content_fixture();
        let sh = content_mode(env);
        run(&sh, false);

        let c = crate::index::open_ro(&sh.env).unwrap();
        let all = crate::index::search(&c, "", "all", "", 100, 0).unwrap();
        assert!(all["total"].as_i64().unwrap() >= 2);

        let treffer = crate::index::search(&c, "irgendwas", "all", "", 100, 0).unwrap();
        assert_eq!(treffer["total"], 1, "Suche nach dem Dateinamen");

        let dups = crate::index::search(&c, "", "dups", "", 100, 0).unwrap();
        assert_eq!(dups["total"], 2, "beide Seiten des Duplikats");

        let geplant = crate::index::search(&c, "", "planned", "", 100, 0).unwrap();
        assert_eq!(geplant["total"], 1, "nur die vorgemerkte Datei");

        let s = crate::index::summary(&c).unwrap();
        assert_eq!(s["duplicates"], 1, "eine ueberzaehlige Kopie");
        assert!(s["reusable"].is_object(), "der Plan ist uebernehmbar");
    }

    #[test]
    fn index_laesst_sich_verwerfen() {
        let (env, _music, cfg) = content_fixture();
        let sh = content_mode(env);
        run(&sh, false);
        assert!(cfg.join("index.db").is_file());
        crate::index::reset(&sh.env).unwrap();
        assert!(!cfg.join("index.db").is_file(), "Index geloescht");
        assert!(crate::index::open_ro(&sh.env).is_err(), "danach gibt es nichts zu lesen");
    }

    // ───────── Playlists ─────────

    #[test]
    fn playlist_wird_nie_kuerzer_auch_wenn_beide_seiten_drin_stehen() {
        let (env, _music, _cfg) = fixture();
        {
            // mp3 UND flac stehen in derselben Playlist
            let c = Connection::open(&env.db_path).unwrap();
            c.execute("DELETE FROM playlist_tracks", []).unwrap();
            c.execute("INSERT INTO playlist_tracks VALUES (1,'pl1','mp3a')", []).unwrap();
            c.execute("INSERT INTO playlist_tracks VALUES (2,'pl1','flacA')", []).unwrap();
            c.execute("UPDATE playlist SET song_count=2 WHERE id='pl1'", []).unwrap();
        }
        let sh = db_mode(env);
        let db = sh.env.db_path.clone();
        run(&sh, true);
        assert_eq!(sh.stats.errors.load(Relaxed), 0);
        let c = Connection::open(&db).unwrap();
        let n: i64 = c.query_row("SELECT count(*) FROM playlist_tracks WHERE playlist_id='pl1'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2, "die Playlist behaelt beide Plaetze");
        let auf_keeper: i64 = c
            .query_row("SELECT count(*) FROM playlist_tracks WHERE playlist_id='pl1' AND media_file_id='flacA'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(auf_keeper, 2, "beide zeigen jetzt auf die behaltene Datei");
        let cnt: i64 = c.query_row("SELECT song_count FROM playlist WHERE id='pl1'", [], |r| r.get(0)).unwrap();
        assert_eq!(cnt, 2, "song_count stimmt weiterhin");
    }

    #[test]
    fn zusammenfassen_darf_die_playlist_kuerzen() {
        let (env, _music, _cfg) = fixture();
        {
            let c = Connection::open(&env.db_path).unwrap();
            c.execute("DELETE FROM playlist_tracks", []).unwrap();
            c.execute("INSERT INTO playlist_tracks VALUES (1,'pl1','mp3a')", []).unwrap();
            c.execute("INSERT INTO playlist_tracks VALUES (2,'pl1','flacA')", []).unwrap();
        }
        let sh = db_mode(env);
        let db = sh.env.db_path.clone();
        sh.settings.lock().unwrap().playlist_keep_duplicates = false;
        run(&sh, true);
        assert_eq!(sh.stats.errors.load(Relaxed), 0);
        let c = Connection::open(&db).unwrap();
        let n: i64 = c.query_row("SELECT count(*) FROM playlist_tracks WHERE playlist_id='pl1'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "hier ist das Kuerzen ausdruecklich gewollt");
    }

    #[test]
    fn eingelesene_playlist_datei_wird_mitgeschrieben() {
        let (env, music, _cfg) = fixture();
        let m3u = music.join("Mix.m3u");
        // Navidrome-Playlists aus Dateien enthalten oft relative Pfade und Kommentarzeilen
        fs::write(
            &m3u,
            format!("#EXTM3U\n#EXTINF:180,Song One\ng1.mp3\n{}\n", music.join("g2-hi.flac").display()),
        )
        .unwrap();
        {
            let c = Connection::open(&env.db_path).unwrap();
            c.execute("DELETE FROM playlist_tracks", []).unwrap();
            c.execute("INSERT INTO playlist_tracks VALUES (1,'pl1','mp3a')", []).unwrap();
            c.execute(
                "UPDATE playlist SET path=?1, sync=1, song_count=1 WHERE id='pl1'",
                params![m3u.to_string_lossy()],
            )
            .unwrap();
        }
        let sh = db_mode(env);
        run(&sh, true);
        assert_eq!(sh.stats.errors.load(Relaxed), 0);
        assert_eq!(sh.stats.m3u.load(Relaxed), 1, "eine Playlist-Datei angefasst");

        let text = fs::read_to_string(&m3u).unwrap();
        assert!(text.contains("g1.flac"), "der neue Pfad steht in der Datei:\n{text}");
        assert!(!text.contains("g1.mp3"), "der alte Pfad ist weg:\n{text}");
        assert!(text.contains("#EXTM3U") && text.contains("#EXTINF"), "Kommentarzeilen bleiben erhalten");
        assert!(text.lines().any(|l| l.trim() == "g1.flac"), "relativ bleibt relativ");
    }

    #[test]
    fn playlist_datei_ohne_sync_wird_nicht_angefasst() {
        let (env, music, _cfg) = fixture();
        let m3u = music.join("Hand.m3u");
        fs::write(&m3u, "g1.mp3\n").unwrap();
        {
            let c = Connection::open(&env.db_path).unwrap();
            c.execute("UPDATE playlist SET path=?1, sync=0 WHERE id='pl1'", params![m3u.to_string_lossy()]).unwrap();
        }
        let sh = db_mode(env);
        run(&sh, true);
        assert_eq!(fs::read_to_string(&m3u).unwrap(), "g1.mp3\n", "selbst gepflegte Playlists bleiben unberuehrt");
        assert_eq!(sh.stats.m3u.load(Relaxed), 0);
    }

    // ───────── Volltextindex (FTS5) ─────────

    /// Wie viele Eintraege findet Navidromes Suchindex zu diesem Begriff?
    fn fts_treffer(db: &str, wort: &str) -> i64 {
        let c = Connection::open(db).unwrap();
        c.query_row(
            "SELECT count(*) FROM media_file_fts WHERE media_file_fts MATCH ?1",
            params![wort],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn markieren_als_fehlend_laesst_den_suchindex_in_ruhe() {
        let (env, _music, _cfg) = fixture();
        let db = env.db_path.clone();
        assert_eq!(fts_treffer(&db, "one"), 2, "mp3 und flac stehen im Suchindex");
        let sh = db_mode(env);
        assert!(!sh.env.delete_rows, "Standard ist markieren, nicht loeschen");
        run(&sh, true);
        assert_eq!(sh.stats.errors.load(Relaxed), 0);
        // Der FTS-Trigger haengt an Titel- und Sortierfeldern, nicht an `missing` –
        // die Zeile bleibt also im Index, so wie Navidrome es auch selbst macht.
        assert_eq!(fts_treffer(&db, "one"), 2, "Suchindex unveraendert");
    }

    #[test]
    fn zeilen_loeschen_raeumt_den_suchindex_mit_auf() {
        let (mut env, _music, _cfg) = fixture();
        env.delete_rows = true;
        let db = env.db_path.clone();
        assert_eq!(fts_treffer(&db, "one"), 2);
        let sh = db_mode(env);
        run(&sh, true);
        assert_eq!(sh.stats.errors.load(Relaxed), 0);

        let c = Connection::open(&db).unwrap();
        let weg: i64 = c.query_row("SELECT count(*) FROM media_file WHERE id='mp3a'", [], |r| r.get(0)).unwrap();
        assert_eq!(weg, 0, "die Zeile ist wirklich geloescht");
        assert_eq!(fts_treffer(&db, "one"), 1, "der Loesch-Trigger hat den Suchindex nachgezogen");
        // Fremdschluessel mit ON DELETE CASCADE duerfen nichts mitreissen, was wir
        // vorher umgehaengt haben
        let scr: i64 = c.query_row("SELECT count(*) FROM scrobbles WHERE media_file_id='flacA'", [], |r| r.get(0)).unwrap();
        assert_eq!(scr, 1, "die Historie haengt an der behaltenen Datei und ueberlebt");
    }

    #[test]
    fn leere_json_felder_zaehlen_nicht_als_tags() {
        // Navidrome legt lyrics als '[]' und tags als '{}' an. Das sind leere Felder –
        // werden sie als vorhanden gezaehlt, waere die Tag-Bewertung fuer jede Datei gleich.
        let (env, _music, _cfg) = fixture();
        let c = Connection::open(&env.db_path).unwrap();
        let schema = Schema::load(&c).unwrap();
        let sql = format!("SELECT {} FROM media_file WHERE id=?1", schema.tag_flags_expr());
        let flags = |id: &str| -> String { c.query_row(&sql, params![id], |r| r.get(0)).unwrap() };

        let leer = flags("mp3a");
        assert!(!leer.contains('l'), "lyrics='[]' zaehlt nicht: {leer}");
        assert!(!leer.contains('T'), "tags='{{}}' zaehlt nicht: {leer}");

        c.execute("UPDATE media_file SET lyrics='[{\"value\":\"hallo\"}]' WHERE id='mp3a'", []).unwrap();
        assert!(flags("mp3a").contains('l'), "echter Liedtext zaehlt");
    }

    #[test]
    fn schreibgeschuetzte_playlist_datei_wird_vorher_gemeldet() {
        let (env, music, _cfg) = fixture();
        let gut = music.join("Gut.m3u");
        let schlecht = music.join("Schreibgeschuetzt.m3u");
        fs::write(&gut, "g1.mp3\n").unwrap();
        fs::write(&schlecht, "g1.mp3\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&schlecht, fs::Permissions::from_mode(0o444)).unwrap();
        }
        {
            let c = Connection::open(&env.db_path).unwrap();
            c.execute("UPDATE playlist SET path=?1, sync=1 WHERE id='pl1'", params![gut.to_string_lossy()]).unwrap();
            c.execute(
                "INSERT INTO playlist (id, name, owner_id, path, sync) VALUES ('pl2','Zweite','u1',?1,1)",
                params![schlecht.to_string_lossy()],
            )
            .unwrap();
            c.execute(
                "INSERT INTO playlist (id, name, owner_id, path, sync) VALUES ('pl3','Weg','u1','/gibt/es/nicht.m3u',1)",
                [],
            )
            .unwrap();
        }
        let sh = db_mode(env);
        run(&sh, false); // schon der Dry-Run muss warnen
        let log: String = sh.log.lock().unwrap().1.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n");
        assert!(log.contains("nicht schreibbar"), "schreibgeschuetzte Datei gemeldet:\n{log}");
        assert!(log.contains("nicht gefunden"), "fehlende Datei gemeldet:\n{log}");
        assert!(log.contains("ACHTUNG"), "zusammenfassende Warnung:\n{log}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&schlecht, fs::Permissions::from_mode(0o644));
        }
    }

    #[test]
    fn beschreibbare_playlist_dateien_werden_bestaetigt() {
        let (env, music, _cfg) = fixture();
        let m3u = music.join("Ok.m3u");
        fs::write(&m3u, "g1.mp3\n").unwrap();
        {
            let c = Connection::open(&env.db_path).unwrap();
            c.execute("UPDATE playlist SET path=?1, sync=1 WHERE id='pl1'", params![m3u.to_string_lossy()]).unwrap();
        }
        let sh = db_mode(env);
        run(&sh, false);
        let log: String = sh.log.lock().unwrap().1.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n");
        assert!(log.contains("beschreibbar"), "Bestaetigung im Protokoll:\n{log}");
        assert!(!log.contains("ACHTUNG"), "keine Warnung noetig:\n{log}");
    }

    #[test]
    fn playlist_m3u8_im_albumordner_wie_in_der_praxis() {
        // Nachgebaut nach einer echten Installation: pro Albumordner eine
        // playlist.m3u8 mit relativen Dateinamen, dazu eine im Wurzelordner
        // mit absoluten Pfaden.
        let (env, music, _cfg) = fixture();
        let album = music.join("Artemas/i like the way you kiss me");
        fs::create_dir_all(&album).unwrap();
        fs::write(album.join("01 Kuss.mp3"), vec![b'k'; 300]).unwrap();
        fs::write(album.join("01 Kuss.flac"), vec![b'K'; 900]).unwrap();
        let im_album = album.join("playlist.m3u8");
        fs::write(&im_album, "#EXTM3U\n#EXTINF:200,Kuss\n01 Kuss.mp3\n").unwrap();
        let wurzel = music.join("playlist.m3u8");
        fs::write(&wurzel, format!("#EXTM3U\n{}\n", album.join("01 Kuss.mp3").display())).unwrap();

        {
            let c = Connection::open(&env.db_path).unwrap();
            let ins = "INSERT INTO media_file (id, library_id, path, title, album, album_id, artist_id, disc_number, \
                       order_title, suffix, size, duration, bit_rate, comment, track_number, sample_rate, bit_depth, \
                       channels, mbz_recording_id, tags, missing, genre) \
                       VALUES (?1, 1, ?2, 'Kuss', 'Kuss-EP', 'albK', 'art1', 1, 'kuss', ?3, ?4, 200.0, ?5, '', 1, 44100, ?6, 2, '', '{}', 0, 'Pop')";
            c.execute(ins, params!["kussmp3", "Artemas/i like the way you kiss me/01 Kuss.mp3", "mp3", 300, 320, 0]).unwrap();
            c.execute(ins, params!["kussflac", "Artemas/i like the way you kiss me/01 Kuss.flac", "flac", 900, 900, 16]).unwrap();
            c.execute("INSERT INTO playlist (id,name,owner_id,path,sync) VALUES ('plA','playlist','u1',?1,1)",
                      params![im_album.to_string_lossy()]).unwrap();
            c.execute("INSERT INTO playlist (id,name,owner_id,path,sync) VALUES ('plR','playlist','u1',?1,1)",
                      params![wurzel.to_string_lossy()]).unwrap();
            c.execute("INSERT INTO playlist_tracks VALUES (1,'plA','kussmp3')", []).unwrap();
            c.execute("INSERT INTO playlist_tracks VALUES (1,'plR','kussmp3')", []).unwrap();
        }
        let sh = db_mode(env);
        run(&sh, true);
        assert_eq!(sh.stats.errors.load(Relaxed), 0);
        assert!(!album.join("01 Kuss.mp3").exists(), "die mp3 ist weg");
        assert!(album.join("01 Kuss.flac").exists(), "die flac bleibt");
        assert_eq!(sh.stats.m3u.load(Relaxed), 2, "beide Playlist-Dateien angefasst");

        let a = fs::read_to_string(&im_album).unwrap();
        assert!(a.lines().any(|l| l.trim() == "01 Kuss.flac"), "relativ bleibt relativ:\n{a}");
        assert!(a.contains("#EXTINF:200,Kuss"), "Kommentarzeile erhalten:\n{a}");

        let w = fs::read_to_string(&wurzel).unwrap();
        assert!(w.contains("01 Kuss.flac"), "absoluter Pfad ersetzt:\n{w}");
        assert!(!w.contains("01 Kuss.mp3"), "alter Pfad weg:\n{w}");
    }

    #[test]
    fn playlist_in_fremder_kodierung_bleibt_unangetastet() {
        let (env, music, _cfg) = fixture();
        let m3u = music.join("Alt.m3u");
        // Latin-1: "Grün.mp3" – kein gueltiges UTF-8
        let mut bytes = b"g1.mp3\nGr".to_vec();
        bytes.push(0xFC);
        bytes.extend_from_slice(b"n.mp3\n");
        fs::write(&m3u, &bytes).unwrap();
        {
            let c = Connection::open(&env.db_path).unwrap();
            c.execute("UPDATE playlist SET path=?1, sync=1 WHERE id='pl1'", params![m3u.to_string_lossy()]).unwrap();
            c.execute("DELETE FROM playlist_tracks", []).unwrap();
            c.execute("INSERT INTO playlist_tracks VALUES (1,'pl1','mp3a')", []).unwrap();
        }
        let sh = db_mode(env);
        run(&sh, true);
        assert_eq!(fs::read(&m3u).unwrap(), bytes, "die Datei wurde nicht angefasst");
        let log: String = sh.log.lock().unwrap().1.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n");
        assert!(log.contains("nicht UTF-8"), "deutliche Meldung statt stiller Beschaedigung:\n{log}");
    }

    #[test]
    fn taktgeber_meldet_sich_nur_in_abstaenden() {
        let mut t = Takt::new(0);
        assert!(t.faellig(), "ohne Wartezeit sofort faellig");
        let mut t = Takt::new(3600);
        assert!(!t.faellig(), "frisch angelegt ist noch nichts faellig");
        assert!(!t.faellig(), "und bleibt es auch");
    }

    // ───────── Angriffe auf den Loeschpfad ─────────
    //
    // Diese Tests bauen von Hand einen Plan in den Index und lassen ihn ueber
    // "Plan uebernehmen" durch den echten Loeschpfad laufen. So laesst sich
    // pruefen, was bei einem Plan passiert, der nicht mehr zur Wirklichkeit passt.

    fn plan_einschleusen(env: &Env, rows: &[(&str, String, &str, String, i64)]) {
        let c = crate::index::open(env).unwrap();
        c.execute(
            "INSERT INTO runs (id, started, ended, mode, execute, complete) \
             VALUES (99, '2026-01-01 00:00:00', '2026-01-01 00:00:01', 'db', 0, 1)",
            [],
        )
        .unwrap();
        for (ki, kp, di, dp, size) in rows {
            crate::index::add_plan(&c, 99, ki, kp, di, dp, *size, "test").unwrap();
        }
    }

    fn mit_plan(env: Env) -> Shared {
        let sh = db_mode(env);
        let mut st = sh.settings.lock().unwrap();
        st.reuse_plan = true;
        drop(st);
        sh
    }

    #[test]
    fn plan_ohne_keeper_eintrag_loescht_nicht() {
        // Die zu loeschende Datei kennt Navidrome, die zu behaltende nicht.
        // Wuerde hier geloescht, blieben Favoriten auf einer Zeile liegen,
        // deren Datei es nicht mehr gibt.
        let (env, music, _cfg) = fixture();
        let flac = music.join("g1.flac").to_string_lossy().to_string();
        let mp3 = music.join("g1.mp3").to_string_lossy().to_string();
        plan_einschleusen(&env, &[("", flac, "mp3a", mp3, 300)]);
        let db = env.db_path.clone();
        let sh = mit_plan(env);
        run(&sh, true);

        assert!(music.join("g1.mp3").exists(), "ohne Ziel fuer die Favoriten darf nichts geloescht werden");
        let c = Connection::open(&db).unwrap();
        let n: i64 = c.query_row("SELECT count(*) FROM annotation WHERE item_id='mp3a'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2, "die Favoriten liegen unveraendert auf der mp3");
        assert!(sh.stats.errors.load(Relaxed) > 0, "und es gibt eine Fehlermeldung");
    }

    #[test]
    fn plan_mit_verschwundenem_keeper_eintrag_loescht_nicht() {
        // Zwischen Dry-Run und Ausfuehren hat Navidrome die behaltene Zeile entfernt.
        let (env, music, _cfg) = fixture();
        let flac = music.join("g1.flac").to_string_lossy().to_string();
        let mp3 = music.join("g1.mp3").to_string_lossy().to_string();
        plan_einschleusen(&env, &[("flacA", flac, "mp3a", mp3, 300)]);
        {
            let c = Connection::open(&env.db_path).unwrap();
            c.execute("DELETE FROM media_file WHERE id='flacA'", []).unwrap();
        }
        let db = env.db_path.clone();
        let sh = mit_plan(env);
        run(&sh, true);

        assert!(music.join("g1.mp3").exists(), "ohne Zielzeile darf nichts geloescht werden");
        let c = Connection::open(&db).unwrap();
        let n: i64 = c.query_row("SELECT count(*) FROM annotation WHERE item_id='mp3a'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2, "Favoriten unveraendert");
    }

    #[test]
    fn nach_dem_plan_geaenderte_datei_wird_nicht_geloescht() {
        // Zwischen Dry-Run und Ausfuehren wurde die Datei ersetzt – der Plan
        // beschreibt sie nicht mehr.
        let (env, music, _cfg) = fixture();
        let flac = music.join("g1.flac").to_string_lossy().to_string();
        let mp3 = music.join("g1.mp3").to_string_lossy().to_string();
        plan_einschleusen(&env, &[("flacA", flac, "mp3a", mp3, 300)]);
        // andere Groesse als im Plan vermerkt
        fs::write(music.join("g1.mp3"), vec![b'n'; 4242]).unwrap();
        let sh = mit_plan(env);
        run(&sh, true);
        assert!(music.join("g1.mp3").exists(), "veraenderte Datei bleibt liegen");
        assert_eq!(sh.stats.deleted.load(Relaxed), 0);
    }

    #[test]
    fn unveraenderter_navidrome_bestand_wird_nicht_neu_eingelesen() {
        let (env, _music, _cfg) = content_fixture();
        {
            let c = Connection::open(&env.db_path).unwrap();
            c.execute("UPDATE media_file SET updated_at='2026-01-01 10:00:00'", []).unwrap();
        }
        let sh = content_mode(env);
        run(&sh, false);
        let protokoll = |sh: &Shared| -> String {
            sh.log.lock().unwrap().1.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n")
        };
        assert!(protokoll(&sh).contains("Navidrome-Eintraege zugeordnet"), "erster Lauf liest ein");

        run(&sh, false);
        assert!(protokoll(&sh).contains("Bestand unveraendert"), "zweiter Lauf nimmt die Zuordnung aus dem Index");

        // Sobald Navidrome etwas anfasst, wird wieder eingelesen
        {
            let c = Connection::open(&sh.env.db_path).unwrap();
            c.execute("UPDATE media_file SET updated_at='2026-02-02 11:00:00' WHERE id='album1'", []).unwrap();
        }
        run(&sh, false);
        assert!(protokoll(&sh).contains("Navidrome-Eintraege zugeordnet"), "geaenderter Bestand wird neu eingelesen");
    }

    #[test]
    fn schutz_begriffe_greifen_auch_ohne_tags_im_index() {
        // Der Tag-Block liegt nicht mehr im Index, sondern wird fuer Kandidaten
        // nachgeladen. Der Schutz muss trotzdem wirken.
        let (env, music, _cfg) = content_fixture();
        {
            let c = Connection::open(&env.db_path).unwrap();
            c.execute(
                "UPDATE media_file SET tags='{\"mood\":[{\"value\":\"Karaoke Version\"}]}' WHERE id='dl1'",
                [],
            )
            .unwrap();
        }
        let sh = content_mode(env);
        sh.settings.lock().unwrap().keywords = vec!["karaoke".into()];
        run(&sh, true);
        assert_eq!(sh.stats.protected.load(Relaxed), 1, "Schutz-Begriff aus dem Tag-Block erkannt");
        assert!(music.join("Downloads/irgendwas.flac").exists(), "geschuetzt, also nicht geloescht");
    }

    #[test]
    fn schreibgeschuetzte_bibliothek_wird_vorher_gemeldet() {
        let (env, music, _cfg) = fixture();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&music, fs::Permissions::from_mode(0o555)).unwrap();
        }
        let sh = db_mode(env);
        run(&sh, false); // schon der Dry-Run muss warnen
        let log: String = sh.log.lock().unwrap().1.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&music, fs::Permissions::from_mode(0o755));
        }
        #[cfg(unix)]
        assert!(log.contains("laesst sich nichts anlegen oder loeschen"), "Warnung fehlt:\n{log}");
    }

    #[test]
    fn beschreibbare_bibliothek_meldet_nichts_und_hinterlaesst_nichts() {
        let (env, music, _cfg) = fixture();
        let sh = db_mode(env);
        run(&sh, false);
        let log: String = sh.log.lock().unwrap().1.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n");
        assert!(!log.contains("laesst sich nichts anlegen"), "keine Warnung noetig:\n{log}");
        let reste: Vec<String> = fs::read_dir(&music)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".nd-dedupe"))
            .collect();
        assert!(reste.is_empty(), "die Testdatei wurde wieder entfernt, gefunden: {reste:?}");
    }

    // ───────── Album-Dubletten: 1:1 oder andere Fassung? ─────────

    /// Legt zwei Dateien mit gleichem Titel im selben Album an.
    /// `extra` passt die zweite Zeile danach an.
    fn album_paar(env: &Env, music: &Path, extra: &str) {
        fs::write(music.join("a1.flac"), vec![b'1'; 900]).unwrap();
        fs::write(music.join("a2.flac"), vec![b'2'; 800]).unwrap();
        let c = Connection::open(&env.db_path).unwrap();
        let ins = "INSERT INTO media_file (id, library_id, path, title, album, album_id, artist_id, \
                   disc_number, order_title, suffix, size, duration, bit_rate, comment, track_number, \
                   sample_rate, bit_depth, channels, mbz_recording_id, mbz_release_track_id, disc_subtitle, \
                   tags, missing, genre) \
                   VALUES (?1, 1, ?2, 'Lied', 'Das Album', 'albX', 'art1', 1, 'lied', 'flac', ?3, 210.0, \
                   900, '', 4, 44100, 16, 2, '', '', '', '{}', 0, 'Pop')";
        c.execute(ins, params!["alpha", "a1.flac", 900]).unwrap();
        c.execute(ins, params!["beta", "a2.flac", 800]).unwrap();
        if !extra.is_empty() {
            c.execute(extra, []).unwrap();
        }
    }

    fn laeuft(env: Env) -> Shared {
        let sh = db_mode(env);
        sh.settings.lock().unwrap().dedupe_flac = true;
        run(&sh, true);
        sh
    }

    #[test]
    fn echte_dublette_im_album_wird_entfernt() {
        let (env, music, _cfg) = fixture();
        album_paar(&env, &music, "");
        let sh = laeuft(env);
        assert!(sh.stats.album_dupes.load(Relaxed) >= 1, "als Album-Dublette gezaehlt");
        assert!(music.join("a1.flac").exists(), "die groessere bleibt");
        assert!(!music.join("a2.flac").exists(), "die Dublette geht");
    }

    #[test]
    fn andere_laufzeit_im_album_ist_eine_andere_fassung() {
        let (env, music, _cfg) = fixture();
        // 2,5 s laenger: innerhalb der allgemeinen Toleranz von 3 s, aber fuer
        // dasselbe Stueck auf demselben Album zu viel
        album_paar(&env, &music, "UPDATE media_file SET duration=212.5 WHERE id='beta'");
        let sh = laeuft(env);
        assert!(music.join("a2.flac").exists(), "bleibt liegen");
        assert!(music.join("a1.flac").exists(), "und die andere auch");
    }

    #[test]
    fn andere_release_track_id_ist_ein_anderer_track() {
        let (env, music, _cfg) = fixture();
        album_paar(&env, &music, "UPDATE media_file SET mbz_release_track_id='aaa' WHERE id='alpha'");
        {
            let c = Connection::open(&env.db_path).unwrap();
            c.execute("UPDATE media_file SET mbz_release_track_id='bbb' WHERE id='beta'", []).unwrap();
        }
        let sh = laeuft(env);
        assert!(music.join("a2.flac").exists(), "MusicBrainz sagt: anderer Track");
        assert_eq!(sh.stats.errors.load(Relaxed), 0);
    }

    #[test]
    fn live_fassung_mit_gleichem_titel_bleibt() {
        let (env, music, _cfg) = fixture();
        // Gleicher Titel, gleiche Laufzeit, gleicher Track – nur der
        // Disc-Untertitel verraet, dass es die Live-Aufnahme ist.
        album_paar(&env, &music, "UPDATE media_file SET disc_subtitle='Live in Hamburg' WHERE id='beta'");
        let sh = laeuft(env);
        assert!(music.join("a2.flac").exists(), "andere Fassung bleibt");
    }

    #[test]
    fn andere_tracknummer_auch_ueber_formatgrenzen_hinweg() {
        // Vorher erlaubt: mp3 und flac mit gleichem Titel im selben Album, aber
        // verschiedenen Tracknummern – typisch fuer Album- und Live-Fassung
        // auf einer Zusammenstellung.
        let (env, music, _cfg) = fixture();
        fs::write(music.join("b1.flac"), vec![b'1'; 900]).unwrap();
        fs::write(music.join("b2.mp3"), vec![b'2'; 300]).unwrap();
        {
            let c = Connection::open(&env.db_path).unwrap();
            let ins = "INSERT INTO media_file (id, library_id, path, title, album, album_id, artist_id, \
                       disc_number, order_title, suffix, size, duration, bit_rate, comment, track_number, \
                       sample_rate, bit_depth, channels, mbz_recording_id, tags, missing, genre) \
                       VALUES (?1, 1, ?2, 'Hit', 'Best Of', 'albB', 'art1', 1, 'hit', ?3, ?4, 210.0, \
                       ?5, '', ?6, 44100, ?7, 2, '', '{}', 0, 'Pop')";
            c.execute(ins, params!["bflac", "b1.flac", "flac", 900, 900, 5, 16]).unwrap();
            c.execute(ins, params!["bmp3", "b2.mp3", "mp3", 300, 320, 12, 0]).unwrap();
        }
        let sh = laeuft(env);
        assert!(music.join("b2.mp3").exists(), "Track 5 und Track 12 sind nicht dasselbe Stueck");
    }

    #[test]
    fn strenge_pruefung_laesst_sich_abschalten() {
        let (env, music, _cfg) = fixture();
        album_paar(&env, &music, "UPDATE media_file SET duration=212.5 WHERE id='beta'");
        let sh = db_mode(env);
        sh.settings.lock().unwrap().strict_album_match = false;
        run(&sh, true);
        assert!(!music.join("a2.flac").exists(), "ohne strenge Pruefung greift nur die allgemeine Toleranz");
    }

    #[test]
    fn versionsmerkmale_werden_erkannt() {
        assert_eq!(version_marks("Lied"), Vec::<&str>::new());
        assert!(version_marks("Lied (Live)").contains(&"live"));
        assert!(version_marks("Lied - Radio Edit").contains(&"radio"));
        assert!(version_marks("Lied (2024 Remaster)").contains(&"remaster"));
        assert_eq!(version_marks("Lied (Live)"), version_marks("LIED live"), "Gross- und Kleinschreibung egal");
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
        let sh = db_mode(env);
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
        let sh = db_mode(env);
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
        let sh = db_mode(env);
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
        let sh = db_mode(env);
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
        let sh = db_mode(env);
        run(&sh, true);
        assert!(music.join("g2-lo.flac").exists(), "andere Recording-ID -> nicht geloescht");
        assert!(!music.join("g1.mp3").exists(), "Gruppe 1 bleibt davon unberuehrt");
    }

    #[test]
    fn flac_dedupe_can_be_disabled() {
        let (env, music, _cfg) = fixture();
        let sh = db_mode(env);
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
        let sh = db_mode(env);
        run(&sh, false);
        assert!(sh.stats.errors.load(Relaxed) > 0, "Fehler gemeldet");
        assert!(!Path::new(&path).exists(), "es wird keine leere DB angelegt");
    }
}
