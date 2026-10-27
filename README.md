# Navidrome Duplicate Tool

Web-UI zum sicheren Entfernen von Duplikaten (MP3 neben FLAC, doppelte FLACs) in einer Navidrome-Bibliothek.
Arbeitet direkt auf der Navidrome-SQLite-DB und uebertraegt **alle nutzerbezogenen Daten** auf die behaltene Datei:
Favoriten, Bewertungen, Wiedergabezaehler, Lesezeichen, Playlist-Eintraege, Scrobble-Historie, Warteschlangen und Shares.

## Installation (Dockge / Docker Compose)
1. `docker-compose.yml` aus diesem Repo als Stack einfuegen (oder den Service in deinen bestehenden Stack kopieren).
2. Pfade und `UI_PASSWORD` anpassen: `/data` = Navidrome-Datenordner (mit `navidrome.db`), `/music` = Musik (read-write, gleicher Pfad wie bei Navidrome).
3. Starten, `http://<server-ip>:8085` oeffnen. Ist `UI_PASSWORD` leer, wird eines erzeugt und im Container-Log angezeigt (gespeichert in `/config/.ui_password`).

## Updates
Das Image wird bei jedem Push auf `main` automatisch per GitHub Actions gebaut und nach `ghcr.io/twil1ghtzone/navidrome-duplicate-tool:latest` veroeffentlicht.
In Dockge genuegt **Aktualisieren** (pull + Neuerstellung). Einmalig noetig: Auf GitHub unter *Profil -> Packages -> navidrome-duplicate-tool ->
Package settings -> Change visibility* das Paket auf **Public** stellen, sonst kann der Server es nicht ohne Login ziehen.
Ohne Registry geht auch der Build direkt aus GitHub (siehe Kommentar in der Compose-Datei, `pull_policy: build`).

## Ablauf: zwei Phasen
1. **Planen** (nur lesend): `media_file` wird sortiert gestreamt, Gruppen gebildet, alle Schutzregeln geprueft und jede
   geplante Loeschung in eine Plan-/Report-Datei unter `/config/reports` geschrieben. Der Speicherbedarf bleibt konstant,
   und es bleibt keine Lese-Transaktion offen (wichtig, damit der SQLite-WAL waehrend langer Laeufe weiter aufgeraeumt wird).
2. **Anwenden** (nur im Execute-Modus): die Plan-Datei wird blockweise abgearbeitet – pro Block eine Transaktion,
   Dateien werden erst nach dem Commit geloescht. Ein Dry-Run fuehrt nur Phase 1 aus.

Das Datenbankschema wird beim Start per `PRAGMA table_info` eingelesen, damit das Tool ueber verschiedene
Navidrome-Versionen hinweg funktioniert (vorhandene Tabellen/Spalten werden genutzt, fehlende uebersprungen).

## Was uebertragen wird (nichts geht verloren)
| Tabelle | Inhalt | Verhalten |
|---|---|---|
| `annotation` | Favorit, Bewertung, Wiedergaben – pro Nutzer | Plays addiert, Rating = Maximum (mit `rated_at`), Favorit bleibt (aeltestes `starred_at`), neuestes `play_date`; danach Verifikation, sonst Rollback |
| `bookmark` | Lesezeichen/Positionen | uebernommen, vorhandene Lesezeichen am Keeper bleiben unveraendert |
| `playlist_tracks` | Playlist-Eintraege | Eintrag wird 1:1 auf die behaltene Datei umgebogen, **Position und Laenge der Playlist bleiben erhalten** (Standard; optional werden Doppelungen stattdessen zusammengefasst). `playlist.duration/size/song_count` werden wie in Navidrome neu berechnet |
| `scrobbles`, `scrobble_buffer` | Wiedergabe-Historie, offene Scrobbles | umgehaengt |
| `playqueue.items`, `share.resource_ids` | ID-Listen | ID wird in der Liste ersetzt |
| `media_file.average_rating` | zwischengespeicherte Durchschnittsbewertung | fuer die behaltene Datei neu berechnet |

Am Ende jeder Transaktion wird geprueft, dass kein `annotation`-, `bookmark`- oder `playlist_tracks`-Eintrag mehr auf die
geloeschte Datei zeigt. Schlaegt die Pruefung fehl, wird der Block zurueckgerollt und keine Datei angefasst.

## Duplikat-Logik und Schutzmechanismen
- Gruppierung nach `album_id + Disc + Titel` (`MATCH_MODE=artist_title` gruppiert albumuebergreifend).
- Behalten wird die beste Datei: zuerst das Wunschformat (`KEEP_EXTS`, Standard FLAC), dann Bit-Tiefe, Samplerate,
  Kanaele, Bitrate und Groesse – bei FLAC/FLAC entscheidet also die echte Qualitaet, nicht nur die Dateigroesse.
- Entfernt werden MP3s (nur wenn eine FLAC existiert) und doppelte Dateien gleichen Formats, z. B. FLAC/FLAC (abschaltbar).
- Uebersprungen wird bei: Schutz-Begriffen (instrumental, karaoke, acapella ... in Titel/Pfad/Album/Kommentar/Tags),
  Exclude-Ordnern, fehlender oder leerer Zieldatei auf Platte, abweichender Laenge (Toleranz einstellbar),
  unterschiedlicher MusicBrainz-Recording-ID, abweichender Tracknummer bei gleichem Format und
  **Hardlinks auf dieselbe Datei** (gleiche Inode).
- **Sicherheitssperre:** "Ausfuehren" ist erst nach einem vollstaendigen Dry-Run (letzte 7 Tage) moeglich (`REQUIRE_DRYRUN=false` schaltet das ab) und verlangt die Eingabe von LOESCHEN.
- **Limit pro Lauf** (z. B. erst 50 Duplikate testen), Backup vor jedem Execute (`VACUUM INTO`), pro Block eine ACID-Transaktion,
  Dateien werden erst nach dem Commit geloescht, Graceful Shutdown beendet immer erst den aktuellen Block.
- Ein fehlerhafter Eintrag bricht keinen Lauf ab: der Block wird zurueckgerollt und jeder Eintrag einzeln erneut versucht,
  der problematische wird uebersprungen und protokolliert.
- Navidrome-Eintraege der geloeschten Dateien werden als `missing` markiert (`DELETE_ROWS=true` loescht sie). Danach in Navidrome einen Scan starten.

## Logs, Verlauf, Backups
- Jeder Lauf schreibt ein vollstaendiges Log (`/config/logs`), eine Plan-Datei `*-plan.tsv` und im Execute-Modus
  zusaetzlich `*-applied.tsv` mit dem tatsaechlichen Ergebnis pro Datei (`/config/reports`), Verlauf in `/config/history.json`.
- Button **Alle Logs als ZIP herunterladen** (Logs, Reports, Verlauf, System-Info). Aufbewahrung: `LOG_KEEP` (Standard 30).
- Backups unter `/backups` (`BACKUP_KEEP`, Standard 3), manuell per Button. Wiederherstellen: Navidrome stoppen, `navidrome.db` (+ `-wal`/`-shm`) durch das Backup ersetzen.

## Umgebungsvariablen (alle optional ausser Pfaden)
`UI_USER` (admin), `UI_PASSWORD`, `NAVIDROME_DB` (/data/navidrome.db), `PATH_MAP` (`/navidrome-pfad:/container-pfad`), `KEEP_EXTS` (flac),
`DELETE_EXTS` (mp3), `DEDUPE_SAME_EXTS` (flac, `none` = aus), `DURATION_TOLERANCE` (3), `BATCH_SIZE` (200), `BACKUP_KEEP` (3),
`MATCH_MODE` (`album` | `artist_title`), `DELETE_ROWS` (false), `REQUIRE_DRYRUN` (true), `LOG_KEEP` (30).
In der UI einstellbar: Toleranz, Limit pro Lauf, Exclude-Ordner, Schutz-Begriffe, Bereinigung gleicher Formate,
Playlist-Verhalten, MusicBrainz-Pruefung und Tracknummer-Pruefung.

## Entwicklung
`cargo test` startet die Integrationstests. Sie bauen eine Navidrome-aehnliche Datenbank samt echten Dateien auf und
pruefen Dry-Run, Execute, Favoriten-/Playlist-/Lesezeichen-/Historien-Uebernahme, Schutzregeln und Qualitaetsauswahl.

Hinweise: DB-Verzeichnis auf lokalem Dateisystem (kein NFS/SMB, wegen WAL). Die UI hat kein TLS: nur im LAN oder hinter Reverse-Proxy betreiben.
CLI: `navidrome-dedupe --headless [--execute]` (einmaliger Lauf ohne UI).
