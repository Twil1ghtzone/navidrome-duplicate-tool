# Navidrome Duplicate Tool

Web-UI zum sicheren Entfernen von Duplikaten (MP3 neben FLAC, doppelte FLACs) in einer Navidrome-Bibliothek.
Arbeitet direkt auf der Navidrome-SQLite-DB und uebertraegt Favoriten, Wiedergabezaehler, Bewertungen und Playlist-Eintraege
auf die behaltene Datei.

## Installation (Dockge / Docker Compose)
1. `docker-compose.yml` aus diesem Repo als Stack einfuegen (oder den Service in deinen bestehenden Stack kopieren).
2. Pfade und `UI_PASSWORD` anpassen: `/data` = Navidrome-Datenordner (mit `navidrome.db`), `/music` = Musik (read-write, gleicher Pfad wie bei Navidrome).
3. Starten, `http://<server-ip>:8085` oeffnen. Ist `UI_PASSWORD` leer, wird eines erzeugt und im Container-Log angezeigt (gespeichert in `/config/.ui_password`).

## Updates
Das Image wird bei jedem Push auf `main` automatisch per GitHub Actions gebaut und nach `ghcr.io/twil1ghtzone/navidrome-duplicate-tool:latest` veroeffentlicht.
In Dockge genuegt **Aktualisieren** (pull + Neuerstellung). Einmalig noetig: Auf GitHub unter *Profil -> Packages -> navidrome-duplicate-tool ->
Package settings -> Change visibility* das Paket auf **Public** stellen, sonst kann der Server es nicht ohne Login ziehen.
Ohne Registry geht auch der Build direkt aus GitHub (siehe Kommentar in der Compose-Datei, `pull_policy: build`).

## Ablauf und Schutzmechanismen
- Gruppierung nach `album_id + Disc + Titel`; die beste Datei bleibt (FLAC zuerst, dann Bitrate/Groesse).
- Entfernt werden MP3s (wenn eine FLAC existiert) und doppelte FLACs (abschaltbar). Bei gleichem Format muss die Tracknummer stimmen.
- Ganze Gruppe wird uebersprungen bei Schutz-Begriffen (instrumental, karaoke, acapella ... in Titel/Pfad/Album/Kommentar/Tags),
  Exclude-Ordnern, fehlender FLAC auf Platte oder abweichender Laenge (Toleranz einstellbar).
- **Sicherheitssperre:** "Ausfuehren" ist erst nach einem vollstaendigen Dry-Run (letzte 7 Tage) moeglich (`REQUIRE_DRYRUN=false` schaltet das ab) und verlangt die Eingabe von LOESCHEN.
- **Limit pro Lauf** (z. B. erst 50 Duplikate testen), Backup vor jedem Execute (`VACUUM INTO`), pro Block eine ACID-Transaktion,
  Dateien werden erst nach dem Commit geloescht, Graceful Shutdown beendet immer erst den aktuellen Block.
- Annotation-Merge: Plays addieren, Rating = Maximum, Favorit bleibt erhalten, aeltestes `starred_at`; danach Verifikation.
- Navidrome-Eintraege der geloeschten Dateien werden als `missing` markiert (`DELETE_ROWS=true` loescht sie). Danach in Navidrome einen Scan starten.

## Logs, Verlauf, Backups
- Jeder Lauf schreibt ein vollstaendiges Log (`/config/logs`) und einen TSV-Report (`/config/reports`), Verlauf in `/config/history.json`.
- Button **Alle Logs als ZIP herunterladen** (Logs, Reports, Verlauf, System-Info). Aufbewahrung: `LOG_KEEP` (Standard 30).
- Backups unter `/backups` (`BACKUP_KEEP`, Standard 3), manuell per Button. Wiederherstellen: Navidrome stoppen, `navidrome.db` (+ `-wal`/`-shm`) durch das Backup ersetzen.

## Umgebungsvariablen (alle optional ausser Pfaden)
`UI_USER` (admin), `UI_PASSWORD`, `NAVIDROME_DB` (/data/navidrome.db), `PATH_MAP` (`/navidrome-pfad:/container-pfad`), `KEEP_EXTS` (flac),
`DELETE_EXTS` (mp3), `DEDUPE_SAME_EXTS` (flac, `none` = aus), `DURATION_TOLERANCE` (3), `BATCH_SIZE` (200), `BACKUP_KEEP` (3),
`MATCH_MODE` (`album` | `artist_title`), `DELETE_ROWS` (false), `REQUIRE_DRYRUN` (true), `LOG_KEEP` (30).
Toleranz, Limit, Exclude-Ordner, Schutz-Begriffe und FLAC-Duplikate sind zusaetzlich in der UI einstellbar.

Hinweise: DB-Verzeichnis auf lokalem Dateisystem (kein NFS/SMB, wegen WAL). Die UI hat kein TLS: nur im LAN oder hinter Reverse-Proxy betreiben.
CLI: `navidrome-dedupe --headless [--execute]` (einmaliger Lauf ohne UI).
