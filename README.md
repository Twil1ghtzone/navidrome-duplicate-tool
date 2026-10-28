# Navidrome Duplicate Tool

Web-UI zum sicheren Entfernen von Duplikaten (MP3 neben FLAC, doppelte FLACs) in einer Navidrome-Bibliothek.
Arbeitet direkt auf der Navidrome-SQLite-DB und uebertraegt **alle nutzerbezogenen Daten** auf die behaltene Datei:
Favoriten, Bewertungen, Wiedergabezaehler, Lesezeichen, Playlist-Eintraege, Scrobble-Historie, Warteschlangen und Shares.

## Installation (Dockge / Docker Compose)
1. `docker-compose.yml` aus diesem Repo als Stack einfuegen (oder den Service in deinen bestehenden Stack kopieren).
2. Pfade und `UI_PASSWORD` anpassen: `/data` = Navidrome-Datenordner (mit `navidrome.db`), `/music` = Musik (read-write, gleicher Pfad wie bei Navidrome).
3. Starten, `http://<server-ip>:8085` oeffnen. Ist `UI_PASSWORD` leer, wird eines erzeugt und im Container-Log angezeigt (gespeichert in `/config/.ui_password`).

## Updates
Das Image wird bei jedem Push auf `main` automatisch gebaut und nach
`ghcr.io/twil1ghtzone/navidrome-duplicate-tool` veroeffentlicht – als `latest`, als Versions-Tag
(z. B. `v0.6.3`) und als Commit-Tag. In Dockge genuegt **Aktualisieren**.

Die mitgelieferte Compose enthaelt `pull_policy: always`. Ohne das kann ein alter lokaler Stand mit
demselben Tag `latest` liegen bleiben, und der Container startet weiter mit der alten Version, obwohl
laengst eine neue veroeffentlicht ist. Genau das ist der haeufigste Grund fuer "aktualisiert, aber
nichts aendert sich".

### Wenn nach dem Update die alte Version laeuft
Drei Befehle, die die Ursache eindeutig eingrenzen:

```
curl -s http://<server-ip>:8085/healthz
docker ps --filter name=navidrome-dedupe --format '{{.Image}}'
docker logs navidrome-dedupe 2>&1 | head -3
```

* `/healthz` meldet die Version des **laufenden Dienstes** (ab v0.3; davor nur `ok`).
  Meldet es die neue Version, hing nur der Browser an einer zwischengespeicherten Seite:
  Strg+Umschalt+R. Ab v0.4.1 liefert der Dienst `Cache-Control: no-store`, damit das nicht mehr passiert.
* Zeigt `docker ps` statt des Image-Namens eine nackte ID wie `e05aa360763e`, laeuft der Container aus
  einem Image **ohne Tag** – praktisch immer ein lokal gebautes. Dann steht in der Compose ein `build:`
  statt `image:`, und kein `pull` der Welt aendert daran etwas.
* Das Container-Log nennt die Version in der ersten Zeile.

Erzwingen laesst sich das Update so (im Stack-Ordner):

```
docker compose down
docker image rm -f ghcr.io/twil1ghtzone/navidrome-duplicate-tool:latest
docker compose pull
docker compose up -d
```

Den Stack-Ordner verraet der Container selbst:

```
docker inspect navidrome-dedupe --format '{{index .Config.Labels "com.docker.compose.project.working_dir"}}'
```

## Eigene Index-Datenbank (`/config/index.db`)
Der Bibliotheks-Scan schreibt alles, was er herausfindet, in eine eigene SQLite-Datenbank: Pfad, Groesse,
Aenderungszeit, Audio-Fingerabdruck, Abtastrate, Bit-Tiefe, Laufzeit und die Zuordnung zu Navidrome
(ID, Tag-Vollstaendigkeit, Titel, Album, Interpret, Track, MusicBrainz-ID). Dazu kommt jeder geplante und
jeder ausgefuehrte Loeschvorgang mit Begruendung und Status.

Aufgeteilt ist der Index in zwei Tabellen: `files` haelt die Tatsachen aus dem Dateisystem, `nd` die Sicht
von Navidrome, verbunden ueber (Bibliothek, relativer Pfad). So muss beim Aktualisieren der einen Seite die
andere nicht angefasst werden.

Daraus ergeben sich drei Dinge:

* **Schnelle Folgelaeufe.** Hat sich eine Datei seit dem letzten Lauf nicht geaendert (gleiche Groesse,
  gleiche Aenderungszeit), kommt ihr Fingerabdruck aus dem Index. Statt die Datei zu oeffnen genuegt ein
  `stat`. In der UI steht, wie viele Dateien so uebernommen wurden.
* **Der Plan ueberlebt den Lauf.** Mit *"beim Ausfuehren den Plan des letzten Dry-Runs uebernehmen"*
  arbeitet der echte Lauf die gespeicherten Entscheidungen ab, ohne die Bibliothek noch einmal zu
  durchsuchen. Vor jedem Loeschen wird geprueft, ob die Datei noch da ist.
* **Durchsuchbar im Browser.** Der Abschnitt **Index** zeigt jede erfasste Datei mit Format, Groesse,
  Laufzeit, Anzahl der Kopien und Status. Suchen laesst sich ueber Pfad, Titel, Album und Interpret,
  filtern nach Duplikaten, exakt bestaetigten Treffern, Dateien ohne Navidrome-Eintrag, vorgemerkten
  Loeschungen und Format.

Alles davon ist abschaltbar (*"eigenen Index fuehren"*). Ohne Index arbeitet der Bibliotheks-Scan wie bisher
auf einer Wegwerf-Datenbank, die nach dem Lauf geloescht wird.

## Zwei Suchverfahren
In der UI unter **Suchverfahren** waehlbar, Standard ist "beides".

### 1. Dateiinhalt (vollstaendiger Bibliotheks-Scan)
Das Tool laeuft selbst durch die Bibliotheksordner – welche das sind, holt es aus Navidromes
`library`-Tabelle – und erkennt Duplikate am Audio selbst, nicht an den Tags. Damit findet es auch,
was eine Suche ueber Album und Titel prinzipiell nicht finden kann:

* identische Dateien in verschiedenen Alben oder Ordnern,
* Kopien mit voellig anderen Tags,
* doppelte MP3s, M4As und alles andere – nicht nur FLAC,
* Dateien, die Navidrome noch gar nicht eingelesen hat (werden als `NICHT IN NAVIDROME` gemeldet).

Indexiert wird jedes Audioformat aus `AUDIO_EXTS` (Standard: flac, mp3, m4a, m4b, aac, alac, ogg, oga,
opus, wma, wav, aiff, aif, ape, wv, mpc, dsf, dff). Eine bytegleiche Kopie ist unabhaengig vom Format eine
Kopie, deshalb gilt die Inhaltssuche fuer alle davon.

Der Verzeichnisdurchlauf prueft in dieser Reihenfolge: Dateiname, Endung, `stat`, und erst dann wird die
Datei geoeffnet. Cover, Logs und Playlists kosten damit nichts, und unveraenderte Dateien kosten nur den
`stat`.

Fuer **FLAC** ist das sowohl exakt als auch guenstig: der STREAMINFO-Block jeder FLAC-Datei enthaelt eine
MD5-Pruefsumme des **unkomprimierten** Audios. Zwei FLACs mit derselben Pruefsumme enthalten garantiert
dasselbe Audio – unabhaengig von Tags, Kompressionsstufe und Dateigroesse. Dafuer werden rund 50 Byte pro
Datei gelesen; es wird nichts dekodiert und nichts durchgehasht.

Fuer andere Formate wird der reine Audiobereich bestimmt (ID3v2, ID3v1 und APE-Tags werden uebersprungen,
zwei MP3s mit unterschiedlichen Tags gelten also als gleich). Gehasht wird nur bei gleicher Audio-Laenge,
und vor dem Loeschen wird der Bereich zusaetzlich **Byte fuer Byte verglichen** (abschaltbar).

Zwischenspeicher ist eine temporaere SQLite-Datei in `/config`, die nach dem Lauf geloescht wird.
Der Arbeitsspeicher bleibt dadurch konstant; plane je nach Bibliotheksgroesse etwas Platz dafuer ein.

### 2. Navidrome-Datenbank
Das bisherige Verfahren: Gruppierung nach `album_id` + Disc + Titel. Es ist schnell, ruehrt das
Dateisystem nicht an und findet als Einziges **formatuebergreifende** Duplikate (MP3 neben FLAC),
weil dort die Dateien naturgemaess nicht byteweise gleich sind.

Es bereinigt ausserdem doppelte Dateien **gleichen** Formats, und zwar fuer jedes Format: zwei FLACs,
zwei MP3s, zwei M4As. Gibt es von einem Titel nur zwei MP3s und keine FLAC, bleibt die bessere
(hoehere Bitrate, vollstaendigere Tags) und die andere geht – mit derselben vollstaendigen Uebernahme
von Favoriten, Playlists und Historie. Steuerbar ueber `DEDUPE_SAME_EXTS` (Standard `*` = alle Formate).

Beide Verfahren schreiben in denselben Plan. Eine Datei, die das erste Verfahren schon zum Loeschen
vorgemerkt hat, wird vom zweiten nicht noch einmal angefasst – und nie als "zu behalten" eingetragen.

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

### Playlists im Besonderen
Eintraege werden **umgebogen, nie geloescht** – auch dann nicht, wenn die behaltene Datei dadurch doppelt in der
Playlist steht. Position und Laenge bleiben also erhalten. Zusaetzlich zaehlt jede Transaktion die aufloesbaren
Eintraege jeder betroffenen Playlist vor und nach der Aenderung: waere eine Playlist kuerzer geworden, wird der
Block verworfen und keine Datei angefasst. (Wer Doppelungen lieber zusammenfassen moechte, schaltet die Option ab –
dann ist das Kuerzerwerden gewollt und die Pruefung entfaellt.)

**Playlists, die Navidrome aus einer Datei einliest** (`sync`, z. B. m3u), sind ein Sonderfall: dort ist die Datei
die Wahrheit, nicht die Datenbank. Beim naechsten Scan wuerde Navidrome die Playlist aus der alten Datei neu
aufbauen und der Eintrag fiele heraus. Das Tool schreibt deshalb den neuen Pfad in die Playlist-Datei mit
(abschaltbar). Kommentarzeilen wie `#EXTM3U` und `#EXTINF` bleiben stehen, relative Pfade bleiben relativ
(passt zu `playlist.m3u8`-Dateien, wie sie viele Downloader je Albumordner anlegen), Symlinks und `..`
werden beim Vergleich aufgeloest, Dateien in anderer Kodierung als UTF-8 werden gar nicht erst angefasst
sondern gemeldet,
und geschrieben wird ueber eine Nachbardatei mit anschliessendem `rename`, damit die Playlist nie halbfertig
auf der Platte liegt. Schon **vor** dem Lauf – also auch im Dry-Run – wird jede eingelesene Playlist-Datei geprueft: gefunden?
beschreibbar? Das Ergebnis steht als `PLAYLIST-WARNUNG` im Protokoll, bevor irgendetwas geloescht wird.
Laesst sich die Datei spaeter nicht schreiben, steht das als Fehler
im Protokoll – die Datenbank ist dann trotzdem korrekt, aber der naechste Navidrome-Scan wuerde den Eintrag
verlieren. Playlists, die du in Navidrome selbst angelegt hast, haben keine Datei und sind davon nicht betroffen.

## Duplikat-Logik und Schutzmechanismen
- Gruppierung nach `album_id + Disc + Titel` (`MATCH_MODE=artist_title` gruppiert albumuebergreifend).
- **Kopien mit Zaehler im Namen** wie `Song (1)`, `Song (2)`, `Song [3]`, `Song - Copy`, `Song Kopie` werden auf
  `Song` normalisiert und damit ueberhaupt erst als Duplikat erkannt (abschaltbar). Echte Klammer-Zusaetze wie
  `Song (Live)` oder `Song (Remix 2)` bleiben unangetastet, und bei gleicher Qualitaet gewinnt die Datei,
  deren Dateiname keine Kopie ist.
- Behalten wird die beste Datei in dieser Reihenfolge: Wunschformat (`KEEP_EXTS`, Standard FLAC), Bit-Tiefe,
  Samplerate, Kanaele, **Tag-Vollstaendigkeit**, "keine Kopie"-Dateiname, dann Bitrate und Groesse.

## Tags
Bei FLAC gegen FLAC entscheidet zuerst die echte Audio-Qualitaet, danach aber die Tags – Bitrate und Dateigroesse
sind bei identischem Audio bedeutungslos, fehlende Tags nicht. Geprueft werden Album-Interpret, Interpret, Album,
Titel, Track- und Disc-Nummer, Jahr, Datum, Genre, Kommentar, Liedtext, MusicBrainz-IDs, Katalognummer, BPM,
Cover, Sortiertitel, Explicit-Kennzeichen, ReplayGain und der Tag-Block von Navidrome.

Pro Titel wird dafuer nur eine ~20 Byte grosse Flag-Zeichenkette aus der Datenbank geholt (ein Zeichen je Feld),
nicht der ganze Tag-Satz – das bleibt auch bei Millionen Titeln guenstig.

Haette die zu loeschende Datei ein Feld, das der behaltenen fehlt, erscheint eine `TAG-WARNUNG` im Protokoll mit
den betroffenen Feldern, der Zaehler "Tag-Warnungen" steigt und der Report vermerkt `tagverlust=...`.
Mit der Option **"nicht loeschen, wenn die Kopie Tag-Felder hat, die der behaltenen Datei fehlen"** wird die
Gruppe stattdessen uebersprungen. Das Tool schreibt selbst keine Tags in Audiodateien.
- Entfernt werden MP3s (nur wenn eine FLAC existiert) und doppelte Dateien gleichen Formats, z. B. FLAC/FLAC (abschaltbar).
- Uebersprungen wird bei: Schutz-Begriffen (instrumental, karaoke, acapella ... in Titel/Pfad/Album/Kommentar/Tags),
  Exclude-Ordnern, fehlender oder leerer Zieldatei auf Platte, abweichender Laenge (Toleranz einstellbar),
  unterschiedlicher MusicBrainz-Recording-ID, abweichender Tracknummer bei gleichem Format,
  **Hardlinks auf dieselbe Datei** (gleiche Inode) und auf Wunsch bei drohendem Tag-Verlust.
- **Sicherheitssperre:** "Ausfuehren" ist erst nach einem vollstaendigen Dry-Run (letzte 7 Tage) moeglich (`REQUIRE_DRYRUN=false` schaltet das ab) und verlangt die Eingabe von LOESCHEN.
- **Limit pro Lauf** (z. B. erst 50 Duplikate testen), Backup vor jedem Execute (`VACUUM INTO`), pro Block eine ACID-Transaktion,
  Dateien werden erst nach dem Commit geloescht, Graceful Shutdown beendet immer erst den aktuellen Block.
- **Papierkorb statt Loeschen** (Option): Dateien wandern mit ihrem Pfad nach `/trash/<Zeitstempel>/...`
  statt entfernt zu werden. Liegt der Papierkorb auf demselben Dateisystem, ist das ein Verschieben
  ohne Kopieraufwand. Zurueckholen ist damit jederzeit moeglich.
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
`MATCH_MODE` (`album` | `artist_title`), `DELETE_ROWS` (false), `REQUIRE_DRYRUN` (true), `LOG_KEEP` (30),
`TRASH_DIR` (/trash), `AUDIO_EXTS` (Formate fuer den Bibliotheks-Scan).
In der UI einstellbar: Toleranz, Limit pro Lauf, Exclude-Ordner, Schutz-Begriffe, Bereinigung gleicher Formate,
Kopie-Erkennung, Tag-Verlust-Sperre, Playlist-Verhalten, MusicBrainz-Pruefung, Tracknummer-Pruefung,
Suchverfahren, Byte-Vergleich, Meldung nicht indizierter Dateien, Papierkorb, Index fuehren,
unveraenderte Dateien ueberspringen und Plan-Uebernahme.

## Oberflaeche
Die laufende Programmversion steht neben dem Namen im Seitenkopf und im Browser-Tab-Titel.
Meldet der Server nach einem Container-Update eine andere Version als die geladene Seite, wird das Feld
hervorgehoben und laedt auf Klick neu. `GET /healthz` antwortet mit `ok navidrome-dedupe <version>`,
`GET /api/status` enthaelt die Version als Feld `version`.

Bewusst schlicht: eine HTML-Seite ohne Framework, ohne Animationen, in Hell und Dunkel.
Oben eine Statuszeile mit Phase, Fortschritt, Durchsatz in Titeln pro Sekunde, Laufzeit und geschaetzter Restzeit
(serverseitig berechnet), darunter alle Zaehler als dichte Liste. Das Protokoll zeigt jede Entscheidung mit
Begruendung, laesst sich nach Text filtern und nach Stufe umschalten (Loeschungen, Uebersprungene,
Warnungen und Fehler) – die Stufenfilter laufen rein ueber CSS, kosten also kein JavaScript pro Zeile.
`/api/status` liefert nur neue Protokollzeilen seit dem letzten Abruf, die Warnungen werden hoechstens
alle 15 Sekunden neu ermittelt, und im Leerlauf wird nur alle 4 Sekunden abgefragt.

## Als Navidrome-Plugin?
Geht nicht sinnvoll. Navidrome-Plugins sind WebAssembly-Module (Extism) mit festen Faehigkeiten –
MetadataAgent, Scrobbler, Lyrics, SonicSimilarity, TaskWorker, Lifecycle, Scheduler- und WebSocket-Callbacks.
Eine eigene Seite in der Navidrome-Oberflaeche ist nicht vorgesehen; sichtbar wird ein Plugin nur als Eintrag
mit Namen, Beschreibung und einem Einstellungsformular. Vor allem bekommen Plugins keinen Zugriff auf die
SQLite-Datenbank: ueber den Library-Dienst gibt es nur Bibliotheks-Kennzahlen und lesenden Dateizugriff.
Die Kernarbeit dieses Tools – Favoriten, Playlists, Lesezeichen und Historie in einer einzigen Transaktion
umhaengen – laesst sich damit nicht abbilden.

Fuer ein integriertes Gefuehl eignet sich ein Reverse-Proxy, der Navidrome unter `/` und dieses Tool unter
z. B. `/dedupe/` ausliefert.

## Entwicklung
`cargo test` startet die Tests (aktuell 43). Das Testschema ist aus dem Schema-Dump einer echten
Navidrome-Installation uebernommen: dieselben Spalten, dieselben Fremdschluessel (mit `PRAGMA foreign_keys=ON`
scharf geschaltet) und die FTS5-Trigger, die bei jedem INSERT, UPDATE und DELETE auf `media_file` mitlaufen.
Zwei Tests pruefen den Suchindex ausdruecklich: als fehlend markieren laesst ihn unberuehrt (der Trigger haengt
an den Titel- und Sortierfeldern, nicht an `missing`), echtes Loeschen der Zeile raeumt ihn ueber den
Loesch-Trigger mit auf. Sie bauen eine Navidrome-aehnliche Datenbank samt echten Dateien auf und
pruefen Dry-Run, Execute, Favoriten-/Playlist-/Lesezeichen-/Historien-Uebernahme, Schutzregeln und Qualitaetsauswahl.

Hinweise: DB-Verzeichnis auf lokalem Dateisystem (kein NFS/SMB, wegen WAL). Die UI hat kein TLS: nur im LAN oder hinter Reverse-Proxy betreiben.
CLI: `navidrome-dedupe --headless [--execute]` (einmaliger Lauf ohne UI).
