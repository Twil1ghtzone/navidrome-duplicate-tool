//! Inhaltsbasierte Duplikatsuche ueber einen vollstaendigen Bibliotheks-Scan.
//!
//! Dieses Verfahren verlaesst sich nicht auf Navidromes Gruppierung nach Album und
//! Titel. Es laeuft selbst durch die Bibliotheksordner – welche das sind, holt es
//! aus Navidromes `library`-Tabelle – und erkennt Duplikate am Audio selbst.
//! Damit findet es auch:
//!
//! * identische Dateien in verschiedenen Alben oder Ordnern,
//! * Kopien mit voellig anderen Tags (Navidrome gruppiert sie nie zusammen),
//! * doppelte MP3s, M4As und alles andere, nicht nur FLAC,
//! * Dateien, die Navidrome noch gar nicht eingelesen hat.
//!
//! Alles Gefundene landet im Index (`/config/index.db`). Beim naechsten Lauf
//! genuegt fuer eine unveraenderte Datei ein `stat` statt sie zu oeffnen.

use crate::audio;
use crate::engine::{
    file_id, is_copy_name, map_root, norm, tag_label, tag_score, tags_missing, Env, Planner, Schema,
    Settings, Shared, Takt,
};
use crate::index;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;

/// Eine Datei aus dem Index, angereichert um das, was Navidrome ueber sie weiss.
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

/// Eine im Verzeichnisdurchlauf gefundene Datei, bevor sie untersucht wurde.
struct Found {
    abs: String,
    rel: String,
    ext: String,
    size: i64,
    mtime: i64,
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
    idx: &mut Connection,
    run: i64,
    planner: &mut Planner,
) -> Result<(), String> {
    // Der Bibliotheks-Scan betrachtet jedes Audioformat: eine bytegleiche Kopie
    // ist unabhaengig vom Format eine Kopie.
    let exts: HashSet<String> = env.audio_exts.iter().cloned().collect();
    if exts.is_empty() {
        return Err("AUDIO_EXTS ist leer – der Bibliotheks-Scan haette nichts zu tun".into());
    }

    // ── 1. Bibliotheken durchlaufen und jede Datei fingerprinten ──
    sh.phase("durchsuche Bibliotheken");
    let mut walked: Vec<i64> = vec![];
    for (id, root) in libs {
        if sh.scan_done() {
            break;
        }
        let base = map_root(env, root);
        sh.log(format!("Bibliothek {id}: durchsuche {base}"));
        walk(sh, settings, idx, run, *id, Path::new(&base), &exts)?;
        walked.push(*id);
    }
    let files = sh.stats.files.load(Relaxed);
    let reused = sh.stats.reused.load(Relaxed);
    sh.log(format!(
        "{files} Audiodateien erfasst, davon {reused} unveraendert aus dem Index uebernommen"
    ));
    if sh.scan_done() {
        return Ok(());
    }

    // Verschwundene Dateien aus dem Index werfen
    if !walked.is_empty() {
        let list = walked.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        let gone = idx
            .execute(&format!("DELETE FROM files WHERE seen <> ?1 AND lib IN ({list})"), params![run])
            .map_err(|e| format!("Index aufraeumen: {e}"))?;
        if gone > 0 {
            sh.log(format!("{gone} Eintraege aus dem Index entfernt (Datei nicht mehr vorhanden)"));
        }
    }

    // ── 2. Navidromes Sicht einlesen ──
    sh.phase("lese Navidrome-Datenbank");
    load_media_files(sh, reader, schema, idx)?;
    let _ = settings;

    let unknown: i64 = idx
        .query_row(
            "SELECT count(*) FROM files f LEFT JOIN nd ON nd.lib=f.lib AND nd.rel=f.rel WHERE nd.id IS NULL",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    sh.stats.unknown.store(unknown.max(0) as u64, Relaxed);
    if unknown > 0 {
        sh.log(format!("{unknown} Dateien liegen im Ordner, sind aber nicht in Navidrome (Scan in Navidrome noetig?)"));
        if settings.report_unknown {
            let mut st = idx
                .prepare(
                    "SELECT f.abs FROM files f LEFT JOIN nd ON nd.lib=f.lib AND nd.rel=f.rel \
                     WHERE nd.id IS NULL ORDER BY f.abs LIMIT 50",
                )
                .map_err(|e| e.to_string())?;
            let it = st.query_map([], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
            for p in it.flatten() {
                sh.log(format!("NICHT IN NAVIDROME: {p}"));
            }
        }
    }

    // ── 3. Gruppen gleicher Fingerabdruecke abarbeiten ──
    sh.phase("vergleiche Dateiinhalte");
    let sql = "SELECT f.fkey, f.abs, f.size, f.astart, f.alen, COALESCE(nd.id,''), \
                      COALESCE(nd.flags,''), COALESCE(nd.title,''), COALESCE(nd.album,''), \
                      COALESCE(nd.comment,'') \
               FROM files f LEFT JOIN nd ON nd.lib = f.lib AND nd.rel = f.rel \
               WHERE f.fkey IN (SELECT fkey FROM files GROUP BY fkey HAVING count(*) > 1) \
               ORDER BY f.fkey";
    let mut st = idx.prepare(sql).map_err(|e| e.to_string())?;
    let mut rows = st.query([]).map_err(|e| e.to_string())?;
    let mut key = String::new();
    let mut group: Vec<Cand> = vec![];
    let mut takt = Takt::new(2);
    let mut gruppen = 0u64;
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
            tags: String::new(), // wird nur fuer Kandidaten nachgeladen
        };
        if k != key {
            handle(sh, reader, schema, settings, &key, std::mem::take(&mut group), planner)?;
            key = k;
            gruppen += 1;
            if takt.faellig() {
                sh.log(format!("… {gruppen} Gruppen gleicher Dateien geprueft"));
            }
        }
        group.push(c);
    }
    if !sh.scan_done() {
        handle(sh, reader, schema, settings, &key, group, planner)?;
    }
    Ok(())
}

// ───────────────────────── Verzeichnisdurchlauf ─────────────────────────

/// Laeuft einen Bibliotheksordner ab. Reihenfolge der Pruefungen ist bewusst so
/// gewaehlt, dass jede Datei moeglichst frueh aussortiert wird:
/// Dateiname → Endung → `stat` → (nur wenn noetig) die Datei oeffnen.
fn walk(
    sh: &Shared,
    settings: &Settings,
    idx: &mut Connection,
    run: i64,
    lib: i64,
    base: &Path,
    exts: &HashSet<String>,
) -> Result<(), String> {
    let mut stack = vec![base.to_path_buf()];
    let mut seen_dirs: HashSet<(u64, u64)> = HashSet::new();
    let mut batch: Vec<Found> = Vec::with_capacity(1024);
    let mut takt = Takt::new(2);
    let mut ordner = 0u64;

    while let Some(dir) = stack.pop() {
        if sh.scan_done() {
            break;
        }
        ordner += 1;
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
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == "@eaDir" || name == "#recycle" || name == "lost+found" {
                continue;
            }
            // file_type() kommt unter Linux aus dem Verzeichniseintrag selbst,
            // kostet also keinen zusaetzlichen Systemaufruf.
            let ft = match ent.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(ent.path());
                continue;
            }
            let is_link = ft.is_symlink();
            if !ft.is_file() && !is_link {
                continue;
            }
            // Endung zuerst pruefen – das sortiert Cover, Logs und Playlists
            // aus, ohne dass dafuer das Dateisystem gefragt werden muss.
            let ext = match name.rfind('.') {
                Some(i) if i + 1 < name.len() => name[i + 1..].to_ascii_lowercase(),
                _ => continue,
            };
            if !exts.contains(&ext) {
                continue;
            }
            let path = ent.path();
            // Symlinks muessen aufgeloest werden, sonst stimmen Groesse und Zeit nicht.
            let md = match if is_link { fs::metadata(&path) } else { ent.metadata() } {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !md.is_file() || md.len() == 0 {
                continue;
            }
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let abs = path.to_string_lossy().to_string();
            let rel = path
                .strip_prefix(base)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| abs.clone());
            batch.push(Found { abs, rel, ext, size: md.len() as i64, mtime });
            // Alle zwei Sekunden eine Zeile: wo sind wir gerade, und wie weit?
            // Die Pruefung haengt an der Datei, nicht am Ordner – sonst bliebe es
            // still, solange ein einzelner riesiger Ordner abgearbeitet wird.
            if takt.faellig() {
                let kurz = dir.strip_prefix(base).unwrap_or(&dir).to_string_lossy().to_string();
                sh.log(format!(
                    "scanne … {} ({} Ordner, {} Dateien, davon {} unveraendert aus dem Index)",
                    if kurz.is_empty() { "Hauptordner".into() } else { kurz },
                    ordner,
                    sh.stats.files.load(Relaxed),
                    sh.stats.reused.load(Relaxed)
                ));
            }
            if batch.len() >= 1000 {
                store(sh, settings, idx, run, lib, &mut batch)?;
            }
        }
    }
    store(sh, settings, idx, run, lib, &mut batch)?;
    sh.log(format!("Bibliothek {lib}: {ordner} Ordner durchlaufen"));
    Ok(())
}

/// Verteilt eine Aufgabe ueber mehrere Threads. Der Bibliotheks-Scan wartet fast
/// nur auf die Platte; nacheinander zu fragen verschenkt auf Netzwerkspeicher und
/// Festplatten den groessten Teil der moeglichen Geschwindigkeit.
fn parallel<T: Sync, R: Send>(threads: usize, items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let n = threads.max(1).min(items.len());
    if n <= 1 {
        return items.iter().map(&f).collect();
    }
    let chunk = items.len().div_ceil(n);
    std::thread::scope(|scope| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|teil| {
                let fref = &f;
                scope.spawn(move || teil.iter().map(fref).collect::<Vec<R>>())
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().expect("Scan-Thread abgestuerzt")).collect()
    })
}

/// Legt einen Stapel gefundener Dateien im Index ab. Unveraenderte Dateien
/// werden nicht erneut untersucht – das ist der eigentliche Zeitgewinn.
fn store(
    sh: &Shared,
    settings: &Settings,
    idx: &mut Connection,
    run: i64,
    lib: i64,
    batch: &mut Vec<Found>,
) -> Result<(), String> {
    if batch.is_empty() {
        return Ok(());
    }
    // 1. Nachsehen, was sich seit dem letzten Lauf nicht geaendert hat
    let mut fresh: Vec<bool> = Vec::with_capacity(batch.len());
    {
        let mut look = idx
            .prepare_cached("SELECT size, mtime FROM files WHERE abs=?1")
            .map_err(|e| e.to_string())?;
        for f in batch.iter() {
            let hit = settings.reuse_index
                && look
                    .query_row(params![f.abs], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                    .map(|(s, m)| s == f.size && m == f.mtime)
                    .unwrap_or(false);
            fresh.push(hit);
        }
    }
    // 2. Nur die uebrigen Dateien wirklich oeffnen – und das parallel,
    //    weil dabei fast nur auf die Platte gewartet wird.
    let offen: Vec<&Found> = batch.iter().zip(fresh.iter()).filter(|(_, h)| !**h).map(|(f, _)| f).collect();
    let ergebnis = parallel(sh.env.scan_threads, &offen, |f| match audio::probe(Path::new(&f.abs)) {
        Ok(i) => Ok(i),
        Err(e) => Err(format!("FEHLER beim Lesen von {}: {e}", f.abs)),
    });
    let mut probed: Vec<Option<audio::Info>> = Vec::with_capacity(batch.len());
    let mut it = ergebnis.into_iter();
    for hit in fresh.iter() {
        if *hit {
            probed.push(None);
            continue;
        }
        match it.next() {
            Some(Ok(i)) => probed.push(Some(i)),
            Some(Err(msg)) => {
                sh.stats.errors.fetch_add(1, Relaxed);
                sh.log(msg);
                probed.push(None);
            }
            None => probed.push(None),
        }
    }
    // 3. In einem Rutsch schreiben
    let tx = idx.transaction().map_err(|e| e.to_string())?;
    {
        let mut touch = tx
            .prepare_cached("UPDATE files SET seen=?2 WHERE abs=?1")
            .map_err(|e| e.to_string())?;
        let mut put = tx
            .prepare_cached(
                "INSERT OR REPLACE INTO files \
                 (abs, lib, rel, ext, size, mtime, fkey, exact, astart, alen, srate, bits, chans, dur, seen) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            )
            .map_err(|e| e.to_string())?;
        for ((f, hit), info) in batch.iter().zip(fresh.iter()).zip(probed.iter()) {
            if *hit {
                touch.execute(params![f.abs, run]).map_err(|e| e.to_string())?;
                sh.stats.reused.fetch_add(1, Relaxed);
            } else if let Some(i) = info {
                put.execute(params![
                    f.abs, lib, f.rel, f.ext, f.size, f.mtime, i.key, i.exact as i64,
                    i.audio_start as i64, i.audio_len as i64,
                    i.sample_rate as i64, i.bits as i64, i.channels as i64, i.duration, run
                ])
                .map_err(|e| e.to_string())?;
            } else {
                continue; // unlesbar – nicht in den Index aufnehmen
            }
            sh.stats.files.fetch_add(1, Relaxed);
            sh.stats.scanned.fetch_add(1, Relaxed);
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    batch.clear();
    Ok(())
}

/// Holt die Navidrome-Eintraege in den Index – ein sequenzieller Durchlauf
/// statt einer Pfadsuche pro Datei. Navidrome hat zwar einen Index auf
/// `media_file.path`, aber ein Durchlauf ist bei Millionen Titeln guenstiger
/// als ebenso viele Einzelabfragen ueber zwei Datenbanken hinweg.
fn load_media_files(sh: &Shared, reader: &Connection, schema: &Schema, idx: &mut Connection) -> Result<(), String> {
    let missing = if schema.mf("missing") { "missing=0 AND " } else { "" };

    // Hat sich Navidromes Bestand seit dem letzten Lauf ueberhaupt veraendert?
    // Zahl der Titel plus juengster Aenderungszeitpunkt genuegen als Fingerabdruck.
    // Stimmt er, sparen wir uns das Neuschreiben von Millionen Zeilen.
    let fp: Option<String> = if schema.mf("updated_at") {
        reader
            .query_row(
                &format!("SELECT count(*) || '/' || COALESCE(max(updated_at),'') FROM media_file WHERE {missing}1=1"),
                [],
                |r| r.get(0),
            )
            .ok()
    } else {
        None
    };
    if let Some(f) = &fp {
        let gefuellt: i64 = idx.query_row("SELECT count(*) FROM nd", [], |r| r.get(0)).unwrap_or(0);
        if gefuellt > 0 && crate::index::get_meta(idx, "nd_fingerprint").as_deref() == Some(f.as_str()) {
            sh.log(format!("Navidrome-Bestand unveraendert ({gefuellt} Titel) – Zuordnung aus dem Index uebernommen"));
            return Ok(());
        }
    }

    let sql = format!(
        "SELECT library_id, path, id, {flags}, title, {album}, {artist}, {track}, {disc}, {mbz}, \
                {brate}, {comment} \
         FROM media_file WHERE {missing}1=1",
        flags = schema.tag_flags_expr(),
        album = schema.expr("album", "''"),
        artist = schema.expr("artist", "''"),
        track = schema.expr("track_number", "0"),
        disc = schema.expr("disc_number", "0"),
        mbz = schema.expr("mbz_recording_id", "''"),
        brate = schema.expr("bit_rate", "0"),
        comment = schema.expr("comment", "''"),
    );
    let mut st = reader.prepare(&sql).map_err(|e| e.to_string())?;
    let mut rows = st.query([]).map_err(|e| e.to_string())?;

    idx.execute("DELETE FROM nd", []).map_err(|e| e.to_string())?;
    let mut n = 0u64;
    let mut takt = Takt::new(2);
    let mut tx = idx.transaction().map_err(|e| e.to_string())?;
    while let Some(r) = rows.next().map_err(|e| e.to_string())? {
        {
            let mut ins = tx
                .prepare_cached(
                    "INSERT OR REPLACE INTO nd (lib, rel, id, flags, title, album, artist, track, disc, mbz, brate, comment) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                )
                .map_err(|e| e.to_string())?;
            let s = |i: usize| r.get::<_, Option<String>>(i).ok().flatten().unwrap_or_default();
            let n64 = |i: usize| r.get::<_, Option<i64>>(i).ok().flatten().unwrap_or(0);
            ins.execute(params![
                n64(0), s(1), s(2), s(3), s(4), s(5), s(6), n64(7), n64(8), s(9), n64(10), s(11)
            ])
            .map_err(|e| e.to_string())?;
        }
        n += 1;
        if n % 50_000 == 0 {
            tx.commit().map_err(|e| e.to_string())?;
            tx = idx.transaction().map_err(|e| e.to_string())?;
        }
        if takt.faellig() {
            sh.log(format!("… {n} Navidrome-Eintraege eingelesen"));
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    if let Some(f) = &fp {
        crate::index::set_meta(idx, "nd_fingerprint", f);
    }
    sh.log(format!("{n} Navidrome-Eintraege zugeordnet"));
    Ok(())
}

// ───────────────────────── Entscheidung ─────────────────────────

/// Entscheidet innerhalb einer Fingerabdruck-Gruppe, was bleibt und was geht.
#[allow(clippy::too_many_arguments)]
fn handle(
    sh: &Shared,
    reader: &Connection,
    schema: &Schema,
    settings: &Settings,
    key: &str,
    mut group: Vec<Cand>,
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
    // Tag-Block erst jetzt nachladen: er wird nur fuer die Schutz-Begriffe
    // gebraucht und waere im Index der mit Abstand groesste Posten.
    if schema.mf("tags") {
        if let Ok(mut st) = reader.prepare_cached("SELECT CAST(tags AS TEXT) FROM media_file WHERE id=?1") {
            for c in group.iter_mut().filter(|c| c.known()) {
                if let Ok(t) = st.query_row(params![c.id], |r| r.get::<_, Option<String>>(0)) {
                    c.tags = t.unwrap_or_default();
                }
            }
        }
    }
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

fn decide(sh: &Shared, settings: &Settings, group: Vec<Cand>, exact: bool, planner: &mut Planner) -> Result<(), String> {
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
                sh.log(format!(
                    "TAG-WARNUNG: {} fehlen der behaltenen Datei {} (aus {})",
                    names.join(", "),
                    keeper.abs,
                    d.abs
                ));
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

/// Nur fuer die Tests: liefert den Pfad des Index.
#[allow(dead_code)]
pub fn index_path(env: &Env) -> PathBuf {
    index::path(env)
}
