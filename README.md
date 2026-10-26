# navidrome-duplicate-tool

Retro-Web-UI (Rust/Axum, Vanilla HTML/JS) zum sicheren Entfernen von MP3-Duplikaten, wenn eine FLAC-Version
desselben Titels existiert. Arbeitet direkt auf der Navidrome-SQLite-DB (`media_file`, `annotation`, `playlist_tracks`).
Getestet gegen das aktuelle Navidrome-Schema (relative Pfade + `library`-Tabelle, `annotation.rated_at`, FTS5-Trigger).

## Funktionsweise
1. `media_file` wird sortiert gestreamt (konstanter RAM, auch bei 10M+ Tracks) und nach
   `album_id + disc_number + order_title` gruppiert (`MATCH_MODE=artist_title` = albumuebergreifend, riskanter).
2. Eine Gruppe wird nur bearbeitet, wenn sie eine Datei aus `KEEP_EXTS` (flac) **und** `DELETE_EXTS` (mp3) enthaelt.
3. Schutzregeln (ganze Gruppe wird uebersprungen): Schutz-Begriffe (instrumental, karaoke, acapella ...) in Titel,
   Album, Kommentar, Pfad oder Tags; Exclude-Ordner; FLAC fehlt/leer auf Platte; MP3-Laenge weicht > `DURATION_TOLERANCE` s ab.
4. Execute: Backup (`VACUUM INTO`) -> pro Block (`BATCH_SIZE`) **eine** `BEGIN IMMEDIATE`-Transaktion:
   Annotationen auf die FLAC-ID mergen (Plays addieren, Rating = Max, Favorit = OR, aeltestes `starred_at`),
   Verifikation, Playlist-Eintraege umbiegen, MP3-Zeile als `missing=1` markieren (oder loeschen mit `DELETE_ROWS=true`).
   **Erst nach Commit** werden die MP3-Dateien geloescht.
5. SIGTERM/Stop: der laufende Block wird sauber abgeschlossen, nie mitten in einer Transaktion abgebrochen.
6. Audit: TSV-Reports unter `config/reports/`, DB-Backups unter `backups/` (die letzten `BACKUP_KEEP`).

## Installation
```bash
git clone https://github.com/Twil1ghtzone/navidrome-duplicate-tool.git && cd navidrome-duplicate-tool
cp .env.example .env && nano .env        # Passwort + Pfade setzen
docker compose up -d --build
```
UI: `http://<server-ip>:8085` (Basic Auth mit `UI_USER`/`UI_PASSWORD`). Reihenfolge: **erst Dry-Run**, Report pruefen, dann Execute.

Wichtig:
- `NAVIDROME_DATA_DIR` muss das **Verzeichnis** mit `navidrome.db` sein (WAL-Dateien `-wal`/`-shm` muessen mit erreichbar sein), lokales Dateisystem, kein NFS/SMB.
- Musikpfad im Container muss dem Pfad der Navidrome-Bibliothek entsprechen (oder `PATH_MAP="/music:/mnt/music"` setzen).
- `PUID/PGID` = Benutzer, dem DB und Musik gehoeren.
- Nach einem Lauf in Navidrome einen Scan starten (Album-Zaehler aktualisieren), danach ggf. "Fehlende Dateien" entfernen.
- Plane Platz fuer das Backup ein (Groesse ~ navidrome.db).

CLI: `navidrome-dedupe --headless [--execute]` (einmaliger Lauf ohne UI; Default = Dry-Run).
Die UI hat keine TLS-Verschluesselung: nur im LAN betreiben oder hinter einen Reverse-Proxy setzen.
