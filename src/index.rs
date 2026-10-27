//! Eigene Index-Datenbank (`/config/index.db`).
//!
//! Der Bibliotheks-Scan schreibt hier alles hinein, was er ueber jede Datei
//! herausgefunden hat: Pfad, Groesse, Aenderungszeit, Audio-Fingerabdruck und
//! die Zuordnung zu Navidrome. Zwei Dinge werden dadurch moeglich:
//!
//! * **Schnelle Folgelaeufe.** Hat sich eine Datei seit dem letzten Lauf nicht
//!   geaendert (gleiche Groesse, gleiche Aenderungszeit), wird ihr Fingerabdruck
//!   aus dem Index genommen. Statt die Datei zu oeffnen genuegt ein `stat`.
//! * **Der Plan ueberlebt den Lauf.** Ein vollstaendiger Dry-Run legt seine
//!   Entscheidungen ab; der anschliessende echte Lauf kann sie uebernehmen,
//!   statt die ganze Bibliothek noch einmal zu durchsuchen.
//!
//! Aufgeteilt ist der Index in zwei Tabellen: `files` haelt die Tatsachen aus
//! dem Dateisystem, `nd` die Sicht von Navidrome. Beide werden ueber
//! (Bibliothek, relativer Pfad) verbunden. So muss beim Aktualisieren der einen
//! Seite die andere nicht angefasst werden.

use crate::engine::Env;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::path::PathBuf;

pub fn path(env: &Env) -> PathBuf {
    env.config_dir.join("index.db")
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    abs    TEXT PRIMARY KEY,
    lib    INTEGER NOT NULL,
    rel    TEXT    NOT NULL,
    ext    TEXT    NOT NULL,
    size   INTEGER NOT NULL,
    mtime  INTEGER NOT NULL,
    fkey   TEXT    NOT NULL,
    exact  INTEGER NOT NULL,
    astart INTEGER NOT NULL,
    alen   INTEGER NOT NULL,
    srate  INTEGER,
    bits   INTEGER,
    chans  INTEGER,
    dur    REAL,
    seen   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS files_key  ON files(fkey);
CREATE INDEX IF NOT EXISTS files_libr ON files(lib, rel);
CREATE INDEX IF NOT EXISTS files_seen ON files(seen);

CREATE TABLE IF NOT EXISTS nd (
    lib   INTEGER NOT NULL,
    rel   TEXT    NOT NULL,
    id    TEXT    NOT NULL,
    flags TEXT    NOT NULL DEFAULT '',
    title TEXT,
    album TEXT,
    artist TEXT,
    track INTEGER,
    disc  INTEGER,
    mbz   TEXT,
    brate INTEGER,
    comment TEXT,
    tags  TEXT,
    PRIMARY KEY (lib, rel)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS runs (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    started  TEXT,
    ended    TEXT,
    mode     TEXT,
    execute  INTEGER NOT NULL DEFAULT 0,
    files    INTEGER NOT NULL DEFAULT 0,
    planned  INTEGER NOT NULL DEFAULT 0,
    complete INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS plan (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    run        INTEGER NOT NULL,
    keeper_id  TEXT,
    keeper_path TEXT,
    del_id     TEXT,
    del_path   TEXT,
    size       INTEGER,
    reason     TEXT,
    status     TEXT NOT NULL DEFAULT 'geplant'
);
CREATE INDEX IF NOT EXISTS plan_run ON plan(run);
CREATE INDEX IF NOT EXISTS plan_del ON plan(del_path);
"#;

/// Oeffnet den Index und legt ihn bei Bedarf an.
pub fn open(env: &Env) -> Result<Connection, String> {
    std::fs::create_dir_all(&env.config_dir).map_err(|e| format!("CONFIG_DIR: {e}"))?;
    open_at(&path(env))
}

/// Dieselbe Struktur an einem beliebigen Ort – fuer den Lauf ohne dauerhaften Index.
pub fn open_at(p: &std::path::Path) -> Result<Connection, String> {
    let c = Connection::open(p).map_err(|e| format!("Index {}: {e}", p.display()))?;
    c.busy_timeout(std::time::Duration::from_secs(30)).map_err(|e| e.to_string())?;
    c.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-131072; PRAGMA temp_store=FILE;",
    )
    .map_err(|e| e.to_string())?;
    c.execute_batch(SCHEMA).map_err(|e| format!("Index-Schema: {e}"))?;
    Ok(c)
}

/// Nur-Lese-Verbindung fuer die Weboberflaeche (blockiert einen laufenden Scan nicht).
pub fn open_ro(env: &Env) -> Result<Connection, String> {
    let p = path(env);
    if !p.is_file() {
        return Err("Es gibt noch keinen Index – zuerst einen Lauf mit Dateiinhalt-Suche starten.".into());
    }
    let c = Connection::open_with_flags(&p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("Index {}: {e}", p.display()))?;
    c.busy_timeout(std::time::Duration::from_secs(10)).map_err(|e| e.to_string())?;
    Ok(c)
}

pub fn start_run(c: &Connection, mode: &str, execute: bool) -> Result<i64, String> {
    // Plaene aelterer Laeufe nicht endlos mitschleppen
    let _ = c.execute(
        "DELETE FROM plan WHERE run NOT IN (SELECT id FROM runs ORDER BY id DESC LIMIT 20)",
        [],
    );
    c.execute(
        "INSERT INTO runs (started, mode, execute) VALUES (?1, ?2, ?3)",
        params![chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(), mode, execute as i64],
    )
    .map_err(|e| e.to_string())?;
    Ok(c.last_insert_rowid())
}

pub fn end_run(c: &Connection, run: i64, files: u64, planned: u64, complete: bool) {
    let _ = c.execute(
        "UPDATE runs SET ended=?2, files=?3, planned=?4, complete=?5 WHERE id=?1",
        params![
            run,
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            files as i64,
            planned as i64,
            complete as i64
        ],
    );
}

/// Letzter vollstaendig durchgelaufener Dry-Run, dessen Plan uebernommen werden kann.
pub fn reusable_plan(c: &Connection) -> Option<(i64, String, i64)> {
    c.query_row(
        "SELECT r.id, r.started, (SELECT count(*) FROM plan WHERE run=r.id) \
         FROM runs r WHERE r.execute=0 AND r.complete=1 \
           AND EXISTS (SELECT 1 FROM plan WHERE run=r.id) ORDER BY r.id DESC LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .ok()
}

/// Merkt eine geplante Loeschung im Index vor.
#[allow(clippy::too_many_arguments)]
pub fn add_plan(
    c: &Connection,
    run: i64,
    keeper_id: &str,
    keeper_path: &str,
    del_id: &str,
    del_path: &str,
    size: i64,
    reason: &str,
) -> Result<(), String> {
    c.execute(
        "INSERT INTO plan (run, keeper_id, keeper_path, del_id, del_path, size, reason) \
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![run, keeper_id, keeper_path, del_id, del_path, size, reason],
    )
    .map_err(|e| format!("Plan speichern: {e}"))?;
    Ok(())
}

/// Haelt im Index fest, was aus einer geplanten Loeschung geworden ist.
pub fn set_status(c: &Connection, run: i64, del_path: &str, status: &str) {
    let _ = c.execute(
        "UPDATE plan SET status=?3 WHERE run=?1 AND del_path=?2",
        params![run, del_path, status],
    );
    if status == "geloescht" || status == "verschoben" {
        let _ = c.execute("DELETE FROM files WHERE abs=?1", params![del_path]);
    }
}

/// Zeilen eines gespeicherten Plans, in der Reihenfolge, in der sie entstanden sind.
#[allow(clippy::type_complexity)]
pub fn plan_rows(c: &Connection, run: i64) -> Result<Vec<(String, String, String, String, i64, String)>, String> {
    let mut st = c
        .prepare("SELECT COALESCE(keeper_id,''), COALESCE(keeper_path,''), COALESCE(del_id,''), \
                         COALESCE(del_path,''), COALESCE(size,0), COALESCE(reason,'') \
                  FROM plan WHERE run=?1 ORDER BY id")
        .map_err(|e| e.to_string())?;
    let it = st
        .query_map(params![run], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .map_err(|e| e.to_string())?;
    Ok(it.flatten().collect())
}

// ───────────────────────── Abfragen fuer die Weboberflaeche ─────────────────────────

/// Welche Teilmenge des Index soll gezeigt werden?
fn filter_sql(only: &str) -> &'static str {
    match only {
        // nur Dateien, die mindestens eine zweite mit gleichem Fingerabdruck haben
        "dups" => " AND f.fkey IN (SELECT fkey FROM files GROUP BY fkey HAVING count(*) > 1)",
        "unknown" => " AND nd.id IS NULL",
        "known" => " AND nd.id IS NOT NULL",
        "exact" => " AND f.exact = 1",
        "planned" => " AND EXISTS (SELECT 1 FROM plan p WHERE p.del_path = f.abs)",
        _ => "",
    }
}

/// Durchsucht den Index. `q` trifft Pfad, Titel, Album und Interpret.
pub fn search(c: &Connection, q: &str, only: &str, ext: &str, limit: i64, offset: i64) -> Result<Value, String> {
    let limit = limit.clamp(1, 500);
    let offset = offset.max(0);
    let like = format!("%{}%", q.trim().to_lowercase());
    let use_q = !q.trim().is_empty();
    let use_ext = !ext.trim().is_empty();

    let mut where_sql = String::from(" WHERE 1=1");
    if use_q {
        where_sql.push_str(
            " AND (lower(f.abs) LIKE ?1 OR lower(COALESCE(nd.title,'')) LIKE ?1 \
                   OR lower(COALESCE(nd.album,'')) LIKE ?1 OR lower(COALESCE(nd.artist,'')) LIKE ?1)",
        );
    }
    if use_ext {
        where_sql.push_str(" AND f.ext = ?2");
    }
    where_sql.push_str(filter_sql(only));

    let base = format!("FROM files f LEFT JOIN nd ON nd.lib = f.lib AND nd.rel = f.rel{where_sql}");
    let ext_l = ext.trim().to_lowercase();

    let total: i64 = {
        let sql = format!("SELECT count(*) {base}");
        let mut st = c.prepare(&sql).map_err(|e| e.to_string())?;
        let get = |r: &rusqlite::Row| r.get::<_, i64>(0);
        match (use_q, use_ext) {
            (true, true) => st.query_row(params![like, ext_l], get),
            (true, false) => st.query_row(params![like], get),
            (false, true) => st.query_row(params![Option::<String>::None, ext_l], get),
            (false, false) => st.query_row([], get),
        }
        .map_err(|e| e.to_string())?
    };

    let sql = format!(
        "SELECT f.abs, f.ext, f.size, f.fkey, f.exact, f.srate, f.bits, f.chans, f.dur, f.lib, \
                COALESCE(nd.id,''), COALESCE(nd.title,''), COALESCE(nd.album,''), COALESCE(nd.artist,''), \
                COALESCE(nd.track,0), COALESCE(nd.brate,0), \
                (SELECT count(*) FROM files g WHERE g.fkey = f.fkey), \
                COALESCE((SELECT p.status FROM plan p WHERE p.del_path = f.abs ORDER BY p.id DESC LIMIT 1),'') \
         {base} ORDER BY f.abs LIMIT {limit} OFFSET {offset}"
    );
    let mut st = c.prepare(&sql).map_err(|e| e.to_string())?;
    let row = |r: &rusqlite::Row| -> rusqlite::Result<Value> {
        Ok(json!({
            "path": r.get::<_, String>(0)?, "ext": r.get::<_, String>(1)?, "size": r.get::<_, i64>(2)?,
            "key": r.get::<_, String>(3)?, "exact": r.get::<_, i64>(4)? == 1,
            "sample_rate": r.get::<_, Option<i64>>(5)?.unwrap_or(0),
            "bits": r.get::<_, Option<i64>>(6)?.unwrap_or(0),
            "channels": r.get::<_, Option<i64>>(7)?.unwrap_or(0),
            "duration": r.get::<_, Option<f64>>(8)?.unwrap_or(0.0),
            "library": r.get::<_, i64>(9)?,
            "id": r.get::<_, String>(10)?, "title": r.get::<_, String>(11)?,
            "album": r.get::<_, String>(12)?, "artist": r.get::<_, String>(13)?,
            "track": r.get::<_, i64>(14)?, "bit_rate": r.get::<_, i64>(15)?,
            "copies": r.get::<_, i64>(16)?, "status": r.get::<_, String>(17)?,
        }))
    };
    let rows: Vec<Value> = match (use_q, use_ext) {
        (true, true) => st.query_map(params![like, ext_l], row).map_err(|e| e.to_string())?.flatten().collect(),
        (true, false) => st.query_map(params![like], row).map_err(|e| e.to_string())?.flatten().collect(),
        (false, true) => st
            .query_map(params![Option::<String>::None, ext_l], row)
            .map_err(|e| e.to_string())?
            .flatten()
            .collect(),
        (false, false) => st.query_map([], row).map_err(|e| e.to_string())?.flatten().collect(),
    };

    Ok(json!({"total": total, "offset": offset, "limit": limit, "rows": rows}))
}

/// Kennzahlen fuer den Kopf der Index-Ansicht.
pub fn summary(c: &Connection) -> Result<Value, String> {
    let one = |sql: &str| -> i64 { c.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    let last = c
        .query_row(
            "SELECT id, started, COALESCE(ended,''), mode, execute, files, planned, complete \
             FROM runs ORDER BY id DESC LIMIT 1",
            [],
            |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?, "started": r.get::<_, String>(1)?, "ended": r.get::<_, String>(2)?,
                    "mode": r.get::<_, String>(3)?, "execute": r.get::<_, i64>(4)? == 1,
                    "files": r.get::<_, i64>(5)?, "planned": r.get::<_, i64>(6)?,
                    "complete": r.get::<_, i64>(7)? == 1
                }))
            },
        )
        .unwrap_or(Value::Null);
    let exts: Vec<Value> = {
        let mut st = c
            .prepare("SELECT ext, count(*) FROM files GROUP BY ext ORDER BY count(*) DESC")
            .map_err(|e| e.to_string())?;
        let it = st
            .query_map([], |r| Ok(json!({"ext": r.get::<_, String>(0)?, "n": r.get::<_, i64>(1)?})))
            .map_err(|e| e.to_string())?;
        it.flatten().collect::<Vec<Value>>()
    };
    Ok(json!({
        "files": one("SELECT count(*) FROM files"),
        "bytes": one("SELECT COALESCE(sum(size),0) FROM files"),
        "duplicates": one("SELECT COALESCE(sum(n-1),0) FROM (SELECT count(*) n FROM files GROUP BY fkey HAVING n>1)"),
        "exact": one("SELECT count(*) FROM files WHERE exact=1"),
        "unknown": one("SELECT count(*) FROM files f LEFT JOIN nd ON nd.lib=f.lib AND nd.rel=f.rel WHERE nd.id IS NULL"),
        // nur der juengste Plan zaehlt, nicht die Summe aller Laeufe
        "planned": one("SELECT count(*) FROM plan WHERE status='geplant' \
                        AND run = (SELECT max(run) FROM plan WHERE status='geplant')"),
        "removed": one("SELECT count(*) FROM plan WHERE status IN ('geloescht','verschoben')"),
        "exts": exts,
        "last_run": last,
        "reusable": reusable_plan(c).map(|(id, started, n)| json!({"run": id, "started": started, "rows": n})),
    }))
}

/// Entfernt den Index vollstaendig (UI-Knopf "Index verwerfen").
pub fn reset(env: &Env) -> Result<(), String> {
    let p = path(env);
    for suffix in ["", "-wal", "-shm"] {
        let f = PathBuf::from(format!("{}{suffix}", p.display()));
        if f.exists() {
            std::fs::remove_file(&f).map_err(|e| format!("{}: {e}", f.display()))?;
        }
    }
    Ok(())
}
