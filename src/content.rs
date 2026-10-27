//! Inhaltsbasierte Duplikatsuche ueber einen vollstaendigen Bibliotheks-Scan.
//!
//! Dieses Verfahren verlaesst sich nicht auf Navidromes Gruppierung nach Album und
//! Titel. Es laeuft stattdessen selbst durch die Bibliotheksordner – welche das sind,
//! holt es aus Navidromes `library`-Tabelle – und erkennt Duplikate am Audio selbst.
//! Damit findet es auch:
//!
//! * identische Dateien in verschiedenen Alben oder Ordnern,
//! * Kopien mit voellig anderen Tags (Navidrome gruppiert sie nie zusammen),
//! * Dateien, die Navidrome noch gar nicht eingelesen hat.
//!
//! Zwischenspeicher ist eine temporaere SQLite-Datei. So bleibt der Arbeitsspeicher
//! konstant, und das Sortieren von Millionen Eintraegen uebernimmt SQLite.

use crate::audio;
use crate::engine::{
    file_id, is_copy_name, map_root, norm, tag_label, tag_score, tags_missing, Env, Planner,
    Schema, Settings, Shared,
};
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;

/// Eine Datei aus dem Bibliotheks-Scan, angereichert um das, was Navidrome ueber sie weiss.
#[derive(Clone)]
struct Cand {
    abs: String,
    size: i64,
    astart: u64,
    alen: u64,
    /// Navidrome-ID, leer wenn die Datei nicht in der Datenbank steht
    id: String,
    flags: String,
    title: String,
    album: String,
    comment: String,
    tags: String,
}

impl Cand {
    fn known(&self) -> bool {
        !self.id.is_empty()
    }
    /// Je groesser, desto eher wird die Datei behalten: zuerst was Navidrome kennt
    /// (damit Favoriten und Playlists ihren Anker behalten), dann Tag-Vollstaendigkeit,
    /// dann ein Dateiname ohne Kopie-Zaehler. Weil das Audio identisch ist, entscheidet
    /// zuletzt die groessere Datei – sie traegt eher eingebettete Cover und Zusatz-Tags.
    fn rank(&self) -> (i64, i64, i64, i64, std::cmp::Reverse<String>) {
        (
            self.known() as i64,
            tag_score(&self.flags),
            !is_copy_name(&self.abs) as i64,
            self.size,
            std::cmp::Reverse(self.abs.clone()),
        )
    }
}

fn tmp_db_path(env: &Env) -> PathBuf {
    env.config_dir.join("scan-index.db")
}

fn drop_tmp(p: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", p.display()));
    }
}

/// Hauptablauf des Inhalts-Scans.
#[allow(clippy::too_many_arguments)]
pub fn scan(
    sh: &Shared,
    env: &Env,
    reader: &Connection,
    schema: &Schema,
    settings: &Settings,
    libs: &HashMap<i64, String>,
    same_exts: &[String],
    planner: &mut Planner,
) -> Result<(), String> {
    let mut exts: Vec<String> = env
        .keep_exts
        .iter()
        .chain(env.delete_exts.iter())
        .chain(same_exts.iter())
        .cloned()
        .collect();
    exts.sort();
    exts.dedup();
    let extset: HashSet<String> = exts.iter().cloned().collect();

    let tmp = tmp_db_path(env);
    drop_tmp(&tmp);
    let mut t = Connection::open(&tmp).map_err(|e| format!("Zwischenspeicher {}: {e}", tmp.display()))?;
    t.execute_batch(
        "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-131072; PRAGMA temp_store=FILE;
         CREATE TABLE f (abs TEXT PRIMARY KEY, lib INTEGER, rel TEXT, size INTEGER,
                         k TEXT, astart INTEGER, alen INTEGER, mf TEXT);
         CREATE TABLE m (lib INTEGER, rel TEXT, id TEXT, flags TEXT, title TEXT, album TEXT,
                         comment TEXT, tags TEXT, PRIMARY KEY (lib, rel)) WITHOUT ROWID;",
    )
    .map_err(|e| e.to_string())?;

    let res = run(sh, env, reader, schema, settings, libs, &extset, &mut t, planner);
    drop(t);
    drop_tmp(&tmp);
    res
}

#[allow(clippy::too_many_arguments)]
fn run(
    sh: &Shared,
    env: &Env,
    reader: &Connection,
    schema: &Schema,
    settings: &Settings,
    libs: &HashMap<i64, String>,
    exts: &HashSet<String>,
    t: &mut Connection,
    planner: &mut Planner,
) -> Result<(), String> {
    // ── 1. Bibliotheken durchlaufen und jede Datei fingerprinten ──
    sh.phase("durchsuche Bibliotheken");
    for (id, root) in libs {
        if sh.scan_done() {
            break;
        }
        let base = map_root(env, root);
        sh.log(format!("Bibliothek {id}: durchsuche {base}"));
        walk(sh, t, *id, Path::new(&base), exts)?;
    }
    let files = sh.stats.files.load(Relaxed);
    sh.log(format!("{files} Audiodateien im Dateisystem gefunden"));
    if sh.scan_done() {
        return Ok(());
    }

    // ── 2. Navidromes Sicht dazuholen (ein Durchlauf, kein Suchen pro Datei) ──
    sh.phase("lese Navidrome-Datenbank");
    load_media_files(sh, reader, schema, exts, t)?;
    t.execute_batch(
        "CREATE INDEX f_k ON f(k);
         UPDATE f SET mf = (SELECT id FROM m WHERE m.lib = f.lib AND m.rel = f.rel);",
    )
    .map_err(|e| format!("Abgleich mit der Datenbank: {e}"))?;

    let unknown: i64 = t.query_row("SELECT count(*) FROM f WHERE mf IS NULL", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    sh.stats.unknown.store(unknown.max(0) as u64, Relaxed);
    if unknown > 0 {
        sh.log(format!("{unknown} Dateien liegen im Ordner, sind aber nicht in Navidrome (Scan in Navidrome noetig?)"));
        if settings.report_unknown {
            let mut st = t.prepare("SELECT abs FROM f WHERE mf IS NULL ORDER BY abs LIMIT 50").map_err(|e| e.to_string())?;
            let it = st.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
            for p in it.flatten() {
                sh.log(format!("NICHT IN NAVIDROME: {p}"));
            }
        }
    }

    // ── 3. Gruppen gleicher Fingerprints abarbeiten ──
    sh.phase("vergleiche Dateiinhalte");
    let sql = "SELECT f.k, f.abs, f.size, f.astart, f.alen, COALESCE(f.mf,''), \
                      COALESCE(m.flags,''), COALESCE(m.title,''), COALESCE(m.album,''), \
                      COALESCE(m.comment,''), COALESCE(m.tags,'') \
               FROM f LEFT JOIN m ON m.lib = f.lib AND m.rel = f.rel \
               WHERE f.k IN (SELECT k FROM f GROUP BY k HAVING count(*) > 1) \
               ORDER BY f.k";
    let mut st = t.prepare(sql).map_err(|e| e.to_string())?;
    let mut rows = st.query([]).map_err(|e| e.to_string())?;
    let mut key = String::new();
    let mut group: Vec<Cand> = vec![];
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        if sh.scan_done() {
            break;
        }
        let k: String = r.get(0).map_err(|e| e.to_string())?;
        let c = Cand {
            abs: r.get(1).map_err(|e| e.to_string())?,
            size: r.get(2).map_err(|e| e.to_string())?,
            astart: r.get::<_, i64>(3).map_err(|e| e.to_string())?.max(0) as u64,
            alen: r.get::<_, i64>(4).map_err(|e| e.to_string())?.max(0) as u64,
            id: r.get(5).map_err(|e| e.to_string())?,
            flags: r.get(6).map_err(|e| e.to_string())?,
            title: r.get(7).map_err(|e| e.to_string())?,
            album: r.get(8).map_err(|e| e.to_string())?,
            comment: r.get(9).map_err(|e| e.to_string())?,
            tags: r.get(10).map_err(|e| e.to_string())?,
        };
        if k != key {
            handle(sh, settings, &key, std::mem::take(&mut group), planner)?;
            key = k;
        }
        group.push(c);
    }
    if !sh.scan_done() {
        handle(sh, settings, &key, group, planner)?;
    }
    Ok(())
}

/// Laeuft einen Bibliotheksordner ab und legt jede Audiodatei mit ihrem Fingerprint ab.
fn walk(sh: &Shared, t: &mut Connection, lib: i64, base: &Path, exts: &HashSet<String>) -> Result<(), String> {
    let mut stack = vec![base.to_path_buf()];
    let mut seen_dirs: HashSet<(u64, u64)> = HashSet::new();
    let mut batch: Vec<(String, String, i64, audio::Info)> = vec![];

    while let Some(dir) = stack.pop() {
        if sh.scan_done() {
            break;
        }
        // Symlink-Schleifen abfangen
        if let Some(id) = file_id(&dir) {
            if !seen_dirs.insert(id) {
                continue;
            }
        }
        let rd = match fs::read_dir(&dir) {
            Ok(x) => x,
            Err(e) => {
                sh.stats.errors.fetch_add(1, Relaxed);
                sh.log(format!("FEHLER beim Lesen von {}: {e}", dir.display()));
                continue;
            }
        };
        for ent in rd.flatten() {
            let path = ent.path();
            let name = ent.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "@eaDir" || name == "#recycle" {
                continue;
            }
            let md = match fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue, // toter Symlink o. ae.
            };
            if md.is_dir() {
                stack.push(path);
                continue;
            }
            let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            if !exts.contains(&ext) || md.len() == 0 {
                continue;
            }
            let info = match audio::probe(&path) {
                Ok(i) => i,
                Err(e) => {
                    sh.stats.errors.fetch_add(1, Relaxed);
                    sh.log(format!("FEHLER beim Lesen von {}: {e}", path.display()));
                    continue;
                }
            };
            let abs = path.to_string_lossy().to_string();
            let rel = path.strip_prefix(base).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| abs.clone());
            batch.push((abs, rel, md.len() as i64, info));
            sh.stats.files.fetch_add(1, Relaxed);
            sh.stats.scanned.fetch_add(1, Relaxed);
            if batch.len() >= 2000 {
                insert(t, lib, &mut batch)?;
            }
        }
    }
    insert(t, lib, &mut batch)
}

fn insert(t: &mut Connection, lib: i64, batch: &mut Vec<(String, String, i64, audio::Info)>) -> Result<(), String> {
    if batch.is_empty() {
        return Ok(());
    }
    let tx = t.transaction().map_err(|e| e.to_string())?;
    {
        let mut st = tx
            .prepare_cached("INSERT OR IGNORE INTO f (abs, lib, rel, size, k, astart, alen) VALUES (?1,?2,?3,?4,?5,?6,?7)")
            .map_err(|e| e.to_string())?;
        for (abs, rel, size, i) in batch.iter() {
            st.execute(params![abs, lib, rel, size, i.key, i.audio_start as i64, i.audio_len as i64])
                .map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    batch.clear();
    Ok(())
}

/// Holt die Navidrome-Eintraege in den Zwischenspeicher – ein sequenzieller Durchlauf
/// statt einer Pfadsuche pro Datei (auf `media_file.path` gibt es keinen Index).
fn load_media_files(
    sh: &Shared,
    reader: &Connection,
    schema: &Schema,
    exts: &HashSet<String>,
    t: &mut Connection,
) -> Result<(), String> {
    let list = exts.iter().map(|e| format!("'{e}'")).collect::<Vec<_>>().join(",");
    let missing = if schema.mf("missing") { "missing=0 AND " } else { "" };
    let sql = format!(
        "SELECT library_id, path, id, {flags}, title, {album}, {comment}, {tags} \
         FROM media_file WHERE {missing}lower(suffix) IN ({list})",
        flags = schema.tag_flags_expr(),
        album = schema.expr("album", "''"),
        comment = schema.expr("comment", "''"),
        tags = if schema.mf("tags") { "CAST(tags AS TEXT)" } else { "''" },
    );
    let mut st = reader.prepare(&sql).map_err(|e| e.to_string())?;
    let mut rows = st.query([]).map_err(|e| e.to_string())?;
    let mut n = 0u64;
    let mut tx = t.transaction().map_err(|e| e.to_string())?;
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        {
            let mut ins = tx
                .prepare_cached("INSERT OR REPLACE INTO m (lib, rel, id, flags, title, album, comment, tags) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)")
                .map_err(|e| e.to_string())?;
            ins.execute(params![
                r.get::<_, Option<i64>>(0).unwrap_or_default().unwrap_or(0),
                r.get::<_, Option<String>>(1).unwrap_or_default().unwrap_or_default(),
                r.get::<_, Option<String>>(2).unwrap_or_default().unwrap_or_default(),
                r.get::<_, Option<String>>(3).unwrap_or_default().unwrap_or_default(),
                r.get::<_, Option<String>>(4).unwrap_or_default().unwrap_or_default(),
                r.get::<_, Option<String>>(5).unwrap_or_default().unwrap_or_default(),
                r.get::<_, Option<String>>(6).unwrap_or_default().unwrap_or_default(),
                r.get::<_, Option<String>>(7).unwrap_or_default().unwrap_or_default(),
            ])
            .map_err(|e| e.to_string())?;
        }
        n += 1;
        if n % 50_000 == 0 {
            tx.commit().map_err(|e| e.to_string())?;
            tx = t.transaction().map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    sh.log(format!("{n} Navidrome-Eintraege zugeordnet"));
    Ok(())
}

/// Entscheidet innerhalb einer Fingerprint-Gruppe, was bleibt und was geht.
fn handle(
    sh: &Shared,
    settings: &Settings,
    key: &str,
    group: Vec<Cand>,
    planner: &mut Planner,
) -> Result<(), String> {
    if group.len() < 2 || key.is_empty() {
        return Ok(());
    }
    let max = settings.max_delete;
    if max > 0 && sh.stats.duplicates.load(Relaxed) >= max {
        if !sh.limit.swap(true, Relaxed) {
            sh.log(format!("Limit von {max} Duplikaten erreicht – Scan wird sauber beendet"));
        }
        return Ok(());
    }
    // `f:` kommt aus der FLAC-Pruefsumme und ist damit bereits beweisend;
    // `s:` heisst nur "gleiche Audio-Laenge" und muss noch geprueft werden.
    let exact = key.starts_with("f:");
    if exact {
        decide(sh, settings, group, true, planner)
    } else {
        for sub in by_hash(sh, group) {
            decide(sh, settings, sub, false, planner)?;
        }
        Ok(())
    }
}

/// Teilt eine Gruppe gleicher Laenge anhand des Audio-Hashes in echte Untergruppen.
fn by_hash(sh: &Shared, group: Vec<Cand>) -> Vec<Vec<Cand>> {
    let mut by: HashMap<u64, Vec<Cand>> = HashMap::new();
    for c in group {
        match audio::hash_audio(Path::new(&c.abs), c.astart, c.alen) {
            Ok(h) => by.entry(h).or_default().push(c),
            Err(e) => {
                sh.stats.errors.fetch_add(1, Relaxed);
                sh.log(format!("FEHLER beim Hashen von {}: {e}", c.abs));
            }
        }
    }
    by.into_values().filter(|v| v.len() > 1).collect()
}

fn excluded(settings: &Settings, abs: &str) -> bool {
    let hay = format!("/{}/", norm(abs).trim_matches('/'));
    settings.excludes.iter().any(|e| {
        let e = norm(e.trim());
        let e = e.trim_matches('/');
        !e.is_empty() && hay.contains(&format!("/{e}/"))
    })
}

fn protected(settings: &Settings, c: &Cand) -> bool {
    let hay = format!("{}\n{}\n{}\n{}\n{}", c.title, c.album, c.comment, c.tags, c.abs).to_lowercase();
    settings.keywords.iter().any(|k| {
        let k = k.trim().to_lowercase();
        !k.is_empty() && hay.contains(&k)
    })
}

fn decide(
    sh: &Shared,
    settings: &Settings,
    group: Vec<Cand>,
    exact: bool,
    planner: &mut Planner,
) -> Result<(), String> {
    if group.len() < 2 {
        return Ok(());
    }
    sh.stats.candidates.fetch_add(1, Relaxed);
    if group.iter().any(|c| excluded(settings, &c.abs)) {
        sh.stats.excluded.fetch_add(1, Relaxed);
        sh.log(format!("SKIP (Exclude-Ordner): {}", group[0].abs));
        return Ok(());
    }
    if group.iter().any(|c| protected(settings, c)) {
        sh.stats.protected.fetch_add(1, Relaxed);
        sh.log(format!("SKIP (Instrumental/Karaoke/Acapella-Schutz): {}", group[0].abs));
        return Ok(());
    }
    let keeper = match group.iter().max_by_key(|c| c.rank()) {
        Some(k) => k.clone(),
        None => return Ok(()),
    };
    let kpath = Path::new(&keeper.abs);
    match fs::metadata(kpath) {
        Ok(m) if m.len() > 0 => {}
        _ => {
            sh.stats.errors.fetch_add(1, Relaxed);
            sh.log(format!("SKIP (zu behaltende Datei fehlt/leer auf Platte): {}", keeper.abs));
            return Ok(());
        }
    }
    let kid = file_id(kpath);

    for d in group.iter().filter(|c| c.abs != keeper.abs) {
        let dpath = Path::new(&d.abs);
        if kid.is_some() && kid == file_id(dpath) {
            sh.stats.mismatch.fetch_add(1, Relaxed);
            sh.log(format!("SKIP (Hardlink auf dieselbe Datei): {}", d.abs));
            continue;
        }
        // Letzte Gewissheit bei nicht-FLAC-Treffern: Byte fuer Byte vergleichen.
        if !exact && settings.verify_bytes {
            match audio::same_audio(kpath, keeper.astart, dpath, d.astart, keeper.alen.min(d.alen)) {
                Ok(true) => {}
                Ok(false) => {
                    sh.stats.mismatch.fetch_add(1, Relaxed);
                    sh.log(format!("SKIP (Byte-Vergleich abweichend): {}", d.abs));
                    continue;
                }
                Err(e) => {
                    sh.stats.errors.fetch_add(1, Relaxed);
                    sh.log(format!("FEHLER beim Vergleich {} : {e}", d.abs));
                    continue;
                }
            }
        }
        // Verliert die behaltene Datei dabei Tags?
        let mut reason = if exact { "inhalt:flac-md5".to_string() } else { "inhalt:bytegleich".to_string() };
        if d.known() && keeper.known() {
            let lost = tags_missing(&d.flags, &keeper.flags);
            if !lost.is_empty() {
                let names: Vec<&str> = lost.iter().copied().map(tag_label).collect();
                reason.push_str(&format!(";tagverlust={}", lost.iter().collect::<String>()));
                sh.stats.tags_lost.fetch_add(1, Relaxed);
                if settings.skip_on_tag_loss {
                    sh.stats.mismatch.fetch_add(1, Relaxed);
                    sh.log(format!("SKIP (Tag-Verlust: {}): {}", names.join(", "), d.abs));
                    continue;
                }
                sh.log(format!("TAG-WARNUNG: {} fehlen der behaltenen Datei {} (aus {})", names.join(", "), keeper.abs, d.abs));
            }
        }
        if !d.known() {
            reason.push_str(";nicht-in-navidrome");
        }
        if !planner.add(sh, &keeper.id, &keeper.abs, &d.id, &d.abs, d.size, &reason)? {
            continue;
        }
        sh.stats.duplicates.fetch_add(1, Relaxed);
        if exact {
            sh.stats.exact.fetch_add(1, Relaxed);
        }
        if !sh.execute.load(Relaxed) {
            sh.stats.bytes_freed.fetch_add(d.size.max(0) as u64, Relaxed);
            sh.log(format!(
                "[PLAN {reason}] behalte {} | loesche {} ({:.1} MB)",
                keeper.abs,
                d.abs,
                d.size as f64 / 1048576.0
            ));
        }
    }
    Ok(())
}
