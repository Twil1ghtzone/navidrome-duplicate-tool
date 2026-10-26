//! Dedupe-Engine: streamt `media_file` sortiert aus der Navidrome-SQLite-DB,
//! bildet Gruppen, überträgt Annotationen und löscht Dateien blockweise in Transaktionen.

use rusqlite::{params, Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{BufWriter, Write};
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
}

fn var(k: &str, d: &str) -> String {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| d.to_string())
}
fn list(k: &str, d: &str) -> Vec<String> {
    var(k, d)
        .split(',')
        .map(|s| s.trim().trim_start_matches('.').to_lowercase())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()))
        .collect()
}

impl Env {
    pub fn from_env() -> Result<Env, String> {
        let ui_user = var("UI_USER", "");
        let ui_pass = var("UI_PASSWORD", "");
        if ui_user.is_empty() || ui_pass.is_empty() {
            return Err("UI_USER und UI_PASSWORD muessen gesetzt sein".into());
        }
        let path_map = std::env::var("PATH_MAP").ok().and_then(|v| {
            v.split_once(':').map(|(a, b)| (a.trim_end_matches('/').to_string(), b.trim_end_matches('/').to_string()))
        });
        Ok(Env {
            db_path: var("NAVIDROME_DB", "/data/navidrome.db"),
            backup_dir: var("BACKUP_DIR", "/backups").into(),
            config_dir: var("CONFIG_DIR", "/config").into(),
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
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub excludes: Vec<String>,
    pub keywords: Vec<String>,
}
impl Default for Settings {
    fn default() -> Self {
        Settings {
            excludes: vec![],
            keywords: ["instrumental", "karaoke", "acapella", "a cappella", "a capella", "accapella"]
                .iter().map(|s| s.to_string()).collect(),
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
    pub protected: AtomicU64,
    pub excluded: AtomicU64,
    pub mismatch: AtomicU64,
    pub deleted: AtomicU64,
    pub bytes_freed: AtomicU64,
    pub annotations: AtomicU64,
    pub errors: AtomicU64,
}

pub struct Shared {
    pub env: Env,
    pub settings: Mutex<Settings>,
    pub stats: Stats,
    pub running: AtomicBool,
    pub stop: AtomicBool,
    pub shutdown: AtomicBool,
    pub execute: AtomicBool,
    pub phase: Mutex<String>,
    pub log: Mutex<(u64, VecDeque<(u64, String)>)>,
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
            execute: AtomicBool::new(false),
            phase: Mutex::new("bereit".into()),
            log: Mutex::new((0, VecDeque::new())),
        }
    }
    pub fn log(&self, msg: impl AsRef<str>) {
        let line = format!("[{}] {}", chrono::Local::now().format("%H:%M:%S"), msg.as_ref());
        eprintln!("{line}");
        let mut g = self.log.lock().unwrap();
        g.0 += 1;
        let seq = g.0;
        g.1.push_back((seq, line));
        while g.1.len() > 1000 {
            g.1.pop_front();
        }
    }
    fn phase(&self, p: &str) {
        *self.phase.lock().unwrap() = p.to_string();
    }
    fn reset_stats(&self) {
        let s = &self.stats;
        for a in [&s.total, &s.scanned, &s.candidates, &s.duplicates, &s.protected, &s.excluded,
                  &s.mismatch, &s.deleted, &s.bytes_freed, &s.annotations, &s.errors] {
            a.store(0, Relaxed);
        }
    }
    fn halt(&self) -> bool {
        self.stop.load(Relaxed) || self.shutdown.load(Relaxed)
    }
}

fn settings_path(env: &Env) -> PathBuf {
    env.config_dir.join("settings.json")
}
fn load_settings(env: &Env) -> Settings {
    fs::read_to_string(settings_path(env)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
pub fn save_settings(env: &Env, s: &Settings) -> std::io::Result<()> {
    fs::create_dir_all(&env.config_dir)?;
    fs::write(settings_path(env), serde_json::to_string_pretty(s).unwrap())
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
    comment: String,
    track: i64,
}

fn s(r: &rusqlite::Row, i: usize) -> rusqlite::Result<String> {
    Ok(r.get::<_, Option<String>>(i)?.unwrap_or_default())
}
fn n(r: &rusqlite::Row, i: usize) -> rusqlite::Result<i64> {
    Ok(r.get::<_, Option<i64>>(i)?.unwrap_or(0))
}
fn f(r: &rusqlite::Row, i: usize) -> rusqlite::Result<f64> {
    Ok(r.get::<_, Option<f64>>(i)?.unwrap_or(0.0))
}

struct Pending {
    keeper: Row,
    dels: Vec<(Row, PathBuf)>,
}

// ───────────────────────── Einstieg ─────────────────────────

pub fn run(sh: &Shared, execute: bool) {
    sh.stop.store(false, Relaxed);
    sh.execute.store(execute, Relaxed);
    sh.reset_stats();
    sh.log(format!("=== START ({}) ===", if execute { "EXECUTE – echte Loeschung" } else { "DRY-RUN – es wird nichts veraendert" }));
    match run_inner(sh, execute) {
        Ok(()) => {
            let msg = if sh.halt() { "abgebrochen/gestoppt (sauber beendet)" } else { "fertig" };
            sh.phase(msg);
            sh.log(format!("=== ENDE: {msg} ==="));
        }
        Err(e) => {
            sh.stats.errors.fetch_add(1, Relaxed);
            sh.phase("FEHLER");
            sh.log(format!("FEHLER: {e}"));
        }
    }
    sh.running.store(false, Relaxed);
}

fn open(env: &Env, query_only: bool) -> Result<Connection, String> {
    let c = Connection::open_with_flags(&env.db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("DB oeffnen ({}): {e} – Pfad/Volume pruefen, es wird absichtlich KEINE neue DB angelegt", env.db_path))?;
    c.busy_timeout(std::time::Duration::from_secs(60)).map_err(|e| e.to_string())?;
    if query_only {
        c.execute_batch("PRAGMA query_only=ON; PRAGMA cache_size=-65536;").map_err(|e| e.to_string())?;
    } else {
        c.execute_batch("PRAGMA foreign_keys=ON;").map_err(|e| e.to_string())?;
    }
    Ok(c)
}

fn run_inner(sh: &Shared, execute: bool) -> Result<(), String> {
    let env = &sh.env;
    let settings = sh.settings.lock().unwrap().clone();
    let reader = open(env, true)?;

    let mut libs: HashMap<i64, String> = HashMap::new();
    {
        let mut st = reader.prepare("SELECT id, path FROM library").map_err(|e| e.to_string())?;
        let it = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
        for x in it {
            let (id, p) = x.map_err(|e| e.to_string())?;
            sh.log(format!("Bibliothek {id}: {p}"));
            libs.insert(id, p);
        }
    }
    let has_rated_at = {
        let mut st = reader.prepare("PRAGMA table_info(annotation)").map_err(|e| e.to_string())?;
        let cols: Vec<String> = st.query_map([], |r| r.get::<_, String>(1)).map_err(|e| e.to_string())?.filter_map(|x| x.ok()).collect();
        cols.iter().any(|c| c == "rated_at")
    };
    let tags_ok = reader.prepare("SELECT CAST(tags AS TEXT) FROM media_file WHERE id=?1").is_ok();

    // Writer + Backup nur im Execute-Modus
    let mut writer = None;
    if execute {
        let w = open(env, false)?;
        sh.phase("Backup");
        let bk = backup(sh, &w)?;
        sh.log(format!("Backup erstellt: {}", bk.display()));
        writer = Some(w);
    }

    // Report-Datei (Audit-Trail)
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let rdir = env.config_dir.join("reports");
    let _ = fs::create_dir_all(&rdir);
    let rpath = rdir.join(format!("{}-{ts}.tsv", if execute { "execute" } else { "dryrun" }));
    let report = fs::File::create(&rpath).ok().map(BufWriter::new);
    sh.log(format!("Report: {}", rpath.display()));

    let ext_sql = |v: &Vec<String>| v.iter().map(|e| format!("'{e}'")).collect::<Vec<_>>().join(",");
    let mut union: Vec<String> = env.keep_exts.iter().chain(env.delete_exts.iter()).chain(env.same_exts.iter()).cloned().collect();
    union.sort();
    union.dedup();
    let all_exts = ext_sql(&union);
    if env.keep_exts.is_empty() || env.delete_exts.is_empty() {
        return Err("KEEP_EXTS/DELETE_EXTS leer".into());
    }

    sh.phase("zaehle Titel");
    let total: i64 = reader
        .query_row(&format!("SELECT count(*) FROM media_file WHERE missing=0 AND lower(suffix) IN ({all_exts})"), [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    sh.stats.total.store(total as u64, Relaxed);
    sh.log(format!("{total} relevante Dateien ({all_exts})"));

    let order = match env.match_mode {
        MatchMode::Album => "album_id, disc_number, order_title",
        MatchMode::ArtistTitle => "artist_id, order_title",
    };
    let sql = format!(
        "SELECT id, library_id, path, title, album, album_id, artist_id, disc_number, order_title, suffix, size, duration, bit_rate, comment, track_number \
         FROM media_file WHERE missing=0 AND lower(suffix) IN ({all_exts}) ORDER BY {order}"
    );

    let mut ctx = Ctx { sh, env, settings, libs, reader: &reader, tags_ok, has_rated_at, writer, batch: vec![], report, execute };
    sh.phase(if execute { "scanne + loesche" } else { "scanne (Dry-Run)" });

    let mut stmt = reader.prepare(&sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut key = String::new();
    let mut group: Vec<Row> = vec![];

    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        if sh.halt() {
            break;
        }
        let row = Row {
            id: s(r, 0).map_err(|e| e.to_string())?,
            library_id: n(r, 1).map_err(|e| e.to_string())?,
            path: s(r, 2).map_err(|e| e.to_string())?,
            title: s(r, 3).map_err(|e| e.to_string())?,
            album: s(r, 4).map_err(|e| e.to_string())?,
            suffix: s(r, 9).map_err(|e| e.to_string())?.to_lowercase(),
            size: n(r, 10).map_err(|e| e.to_string())?,
            duration: f(r, 11).map_err(|e| e.to_string())?,
            bit_rate: n(r, 12).map_err(|e| e.to_string())?,
            comment: s(r, 13).map_err(|e| e.to_string())?,
            track: n(r, 14).map_err(|e| e.to_string())?,
        };
        let (a, b, c, d) = (s(r, 5).unwrap_or_default(), s(r, 6).unwrap_or_default(), n(r, 7).unwrap_or(0), s(r, 8).unwrap_or_default());
        let k = if d.is_empty() {
            format!("\u{0}{}", row.id) // ohne Titel nie gruppieren
        } else if env.match_mode == MatchMode::Album {
            format!("{a}\u{1}{c}\u{1}{d}")
        } else {
            format!("{b}\u{1}{d}")
        };
        sh.stats.scanned.fetch_add(1, Relaxed);
        if k != key {
            ctx.group(std::mem::take(&mut group))?;
            key = k;
        }
        group.push(row);
    }
    if !sh.halt() {
        ctx.group(group)?;
    }
    ctx.flush()?; // laufenden Block immer sauber abschliessen
    if let Some(r) = ctx.report.as_mut() {
        let _ = r.flush();
    }
    drop(rows);
    Ok(())
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

// ───────────────────────── Gruppenlogik ─────────────────────────

struct Ctx<'a> {
    sh: &'a Shared,
    env: &'a Env,
    settings: Settings,
    libs: HashMap<i64, String>,
    reader: &'a Connection,
    tags_ok: bool,
    has_rated_at: bool,
    writer: Option<Connection>,
    batch: Vec<Pending>,
    report: Option<BufWriter<fs::File>>,
    execute: bool,
}

fn norm(p: &str) -> String {
    p.replace('\\', "/").to_lowercase()
}

impl<'a> Ctx<'a> {
    fn full(&self, r: &Row) -> PathBuf {
        let root = self.libs.get(&r.library_id).cloned().unwrap_or_default();
        let root = match &self.env.path_map {
            Some((from, to)) if root.starts_with(from.as_str()) => format!("{to}{}", &root[from.len()..]),
            _ => root,
        };
        Path::new(&root).join(&r.path)
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
        let mut hay = format!("{}\n{}\n{}\n{}", r.title, r.album, r.comment, full.to_string_lossy()).to_lowercase();
        if self.tags_ok {
            if let Ok(mut st) = self.reader.prepare_cached("SELECT CAST(tags AS TEXT) FROM media_file WHERE id=?1") {
                if let Ok(t) = st.query_row([&r.id], |x| x.get::<_, Option<String>>(0)) {
                    hay.push('\n');
                    hay.push_str(&t.unwrap_or_default().to_lowercase());
                }
            }
        }
        self.settings.keywords.iter().any(|k| {
            let k = k.trim().to_lowercase();
            !k.is_empty() && hay.contains(&k)
        })
    }

    fn rep(&mut self, line: &str) {
        if let Some(r) = self.report.as_mut() {
            let _ = writeln!(r, "{line}");
        }
    }

    fn group(&mut self, g: Vec<Row>) -> Result<(), String> {
        if g.len() < 2 {
            return Ok(());
        }
        // Behaltene Datei: bevorzugt aus KEEP_EXTS (z.B. FLAC); gibt es keine, aus den Same-Format-Typen.
        // Entfernt werden: alle DELETE_EXTS-Dateien (nur wenn eine KEEP-Datei existiert) sowie
        // weitere Dateien der DEDUPE_SAME_EXTS-Typen (z.B. doppelte FLACs).
        let rank = |r: &&Row| (r.bit_rate, r.size, std::cmp::Reverse(r.id.clone()));
        let keepers: Vec<&Row> = g.iter().filter(|r| self.env.keep_exts.contains(&r.suffix)).collect();
        let keeper_row: &Row = match keepers.iter().copied().max_by_key(rank) {
            Some(k) => k,
            None => match g.iter().filter(|r| self.env.same_exts.contains(&r.suffix)).max_by_key(rank) {
                Some(k) => k,
                None => return Ok(()),
            },
        };
        let have_keep = !keepers.is_empty();
        let dels: Vec<&Row> = g
            .iter()
            .filter(|r| {
                r.id != keeper_row.id
                    && ((have_keep && self.env.delete_exts.contains(&r.suffix)) || self.env.same_exts.contains(&r.suffix))
            })
            .collect();
        if dels.is_empty() {
            return Ok(());
        }
        let sh = self.sh;
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
                sh.log(format!("SKIP (FLAC fehlt/leer auf Platte): {}", kpath.display()));
                return Ok(());
            }
        }

        let mut ok_dels = vec![];
        for d in dels {
            if (d.duration - keeper.duration).abs() > self.env.duration_tol {
                sh.stats.mismatch.fetch_add(1, Relaxed);
                sh.log(format!("SKIP (Laenge weicht ab {:.1}s vs {:.1}s): {}", d.duration, keeper.duration, d.path));
                continue;
            }
            let dpath = self.full(d);
            if dpath == kpath {
                continue; // dieselbe physische Datei – niemals loeschen
            }
            if d.suffix == keeper.suffix && d.track != keeper.track {
                sh.stats.mismatch.fetch_add(1, Relaxed);
                sh.log(format!("SKIP (Tracknummer {} vs {} bei gleichem Format): {}", d.track, keeper.track, d.path));
                continue;
            }
            ok_dels.push((d.clone(), dpath));
        }
        if ok_dels.is_empty() {
            return Ok(());
        }
        sh.stats.duplicates.fetch_add(ok_dels.len() as u64, Relaxed);

        if !self.execute {
            for (d, _) in &ok_dels {
                sh.stats.bytes_freed.fetch_add(d.size.max(0) as u64, Relaxed);
                sh.log(format!("[DRY] behalte {} | loesche {} ({:.1} MB)", keeper.path, d.path, d.size as f64 / 1048576.0));
                let line = format!("{}\t{}\t{}\t{}", keeper.id, keeper.path, d.id, d.path);
                self.rep(&line);
            }
            return Ok(());
        }
        self.batch.push(Pending { keeper, dels: ok_dels });
        if self.batch.len() >= self.env.batch_size {
            self.flush()?;
        }
        Ok(())
    }

    /// Ein Block = eine ACID-Transaktion. Dateien werden erst NACH erfolgreichem Commit geloescht.
    fn flush(&mut self) -> Result<(), String> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.batch);
        let sh = self.sh;
        let w = self.writer.as_mut().ok_or("kein Writer")?;
        let mut moved = 0u64;
        {
            let tx = w.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|e| e.to_string())?;
            for p in &batch {
                for (d, _) in &p.dels {
                    moved += transfer(&tx, &p.keeper.id, &d.id, self.has_rated_at, self.env.delete_rows)
                        .map_err(|e| format!("Transaktion zurueckgerollt ({}): {e}", d.path))?;
                }
            }
            tx.commit().map_err(|e| format!("Commit fehlgeschlagen: {e}"))?;
        }
        sh.stats.annotations.fetch_add(moved, Relaxed);
        for p in &batch {
            for (d, full) in &p.dels {
                match fs::remove_file(full) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        sh.stats.errors.fetch_add(1, Relaxed);
                        sh.log(format!("FEHLER beim Loeschen {}: {e}", full.display()));
                        continue;
                    }
                }
                sh.stats.deleted.fetch_add(1, Relaxed);
                sh.stats.bytes_freed.fetch_add(d.size.max(0) as u64, Relaxed);
                sh.log(format!("GELOESCHT {} (behalten: {})", d.path, p.keeper.path));
                let line = format!("{}\t{}\t{}\t{}", p.keeper.id, p.keeper.path, d.id, d.path);
                self.rep(&line);
            }
        }
        if let Some(r) = self.report.as_mut() {
            let _ = r.flush();
        }
        Ok(())
    }
}

/// Uebertraegt Annotationen (Favorit, Plays, Rating), Playlist-Eintraege und markiert/loescht die MP3-Zeile.
fn transfer(tx: &rusqlite::Transaction, keeper: &str, del: &str, rated_at: bool, delete_rows: bool) -> rusqlite::Result<u64> {
    let (rc, rv, ru) = if rated_at {
        (", rated_at", ", rated_at", ", rated_at = CASE WHEN excluded.rating > COALESCE(rating,0) THEN excluded.rated_at ELSE rated_at END")
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

    // Verifikation: nichts darf auf der FLAC-ID "schlechter" sein als auf der MP3-ID
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
        return Err(rusqlite::Error::StatementChangedRows(bad as usize));
    }
    tx.execute("DELETE FROM annotation WHERE item_id=?1 AND item_type='media_file'", params![del])?;

    // Playlists: Eintrag auf FLAC umbiegen (ohne Doppelungen), Rest entfernen
    tx.execute(
        "UPDATE playlist_tracks SET media_file_id=?1 WHERE media_file_id=?2 \
         AND playlist_id NOT IN (SELECT playlist_id FROM playlist_tracks WHERE media_file_id=?1)",
        params![keeper, del],
    )?;
    tx.execute("DELETE FROM playlist_tracks WHERE media_file_id=?1", params![del])?;

    if delete_rows {
        tx.execute("DELETE FROM media_file WHERE id=?1", params![del])?;
    } else {
        tx.execute("UPDATE media_file SET missing=1 WHERE id=?1", params![del])?;
    }
    Ok(moved)
}
