//! Audio-Fingerprints direkt aus der Datei – ohne Fremdbibliotheken.
//!
//! Der Trick fuer grosse Bibliotheken: FLAC speichert im STREAMINFO-Block eine
//! MD5-Pruefsumme des **unkomprimierten** Audios. Zwei FLAC-Dateien mit derselben
//! Pruefsumme enthalten garantiert dasselbe Audio – unabhaengig von Tags,
//! Kompressionsstufe oder Dateigroesse. Dafuer genuegen rund 50 gelesene Bytes
//! pro Datei, es muss nichts dekodiert und nichts durchgehasht werden.
//!
//! Fuer andere Formate wird der reine Audiobereich bestimmt (ID3v2/ID3v1/APE
//! werden uebersprungen) und erst bei gleicher Laenge gehasht. Vor dem Loeschen
//! wird dieser Bereich zusaetzlich Byte fuer Byte verglichen.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

/// Ergebnis der Datei-Untersuchung.
#[derive(Clone, Debug, PartialEq)]
pub struct Info {
    /// Gruppierschluessel. `f:<md5>` = garantiert gleiches Audio,
    /// `s:<ext>:<len>` = gleiche Audio-Bytelaenge (muss noch geprueft werden).
    pub key: String,
    /// true, wenn der Schluessel das Audio bereits eindeutig identifiziert.
    pub exact: bool,
    pub audio_start: u64,
    pub audio_len: u64,
    pub sample_rate: u32,
    pub bits: u8,
    pub channels: u8,
    /// Laufzeit in Sekunden, 0.0 wenn unbekannt.
    pub duration: f64,
}

fn hex16(b: &[u8]) -> String {
    let mut s = String::with_capacity(32);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// Groesse eines ID3v2-Blocks am Dateianfang (0 = keiner).
fn id3v2_len(head: &[u8]) -> u64 {
    if head.len() < 10 || &head[0..3] != b"ID3" {
        return 0;
    }
    // Syncsafe-Integer: je 7 nutzbare Bits
    let size = ((head[6] as u64 & 0x7f) << 21)
        | ((head[7] as u64 & 0x7f) << 14)
        | ((head[8] as u64 & 0x7f) << 7)
        | (head[9] as u64 & 0x7f);
    let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
    10 + size + footer
}

/// Laenge der Tags am Dateiende (ID3v1 und/oder APEv2).
fn trailer_len(f: &mut File, len: u64) -> std::io::Result<u64> {
    let mut cut = 0u64;
    if len >= 128 {
        let mut b = [0u8; 128];
        f.seek(SeekFrom::Start(len - 128))?;
        f.read_exact(&mut b)?;
        if &b[0..3] == b"TAG" {
            cut += 128;
        }
    }
    if len - cut >= 32 {
        let mut b = [0u8; 32];
        f.seek(SeekFrom::Start(len - cut - 32))?;
        f.read_exact(&mut b)?;
        if &b[0..8] == b"APETAGEX" {
            let size = u32::from_le_bytes([b[12], b[13], b[14], b[15]]) as u64;
            // Bit 31 der Flags: ein Header liegt zusaetzlich davor
            let flags = u32::from_le_bytes([b[20], b[21], b[22], b[23]]);
            let header = if flags & 0x8000_0000 != 0 { 32 } else { 0 };
            if size + header <= len - cut {
                cut += size + header;
            }
        }
    }
    Ok(cut)
}

/// FLAC-STREAMINFO lesen. Gibt `None` zurueck, wenn die Datei kein FLAC ist
/// oder keine MD5-Summe traegt (alles Nullen heisst laut Spezifikation "unbekannt").
fn flac_streaminfo(f: &mut File, start: u64) -> std::io::Result<Option<Info>> {
    f.seek(SeekFrom::Start(start))?;
    let mut magic = [0u8; 4];
    if f.read_exact(&mut magic).is_err() || &magic != b"fLaC" {
        return Ok(None);
    }
    let mut bh = [0u8; 4];
    f.read_exact(&mut bh)?;
    if bh[0] & 0x7f != 0 {
        return Ok(None); // erster Block ist laut Spezifikation immer STREAMINFO
    }
    let blen = ((bh[1] as usize) << 16) | ((bh[2] as usize) << 8) | bh[3] as usize;
    if blen < 34 {
        return Ok(None);
    }
    let mut b = [0u8; 34];
    f.read_exact(&mut b)?;

    let sample_rate = ((b[10] as u32) << 12) | ((b[11] as u32) << 4) | ((b[12] as u32) >> 4);
    let channels = ((b[12] >> 1) & 0x07) + 1;
    let bits = (((b[12] & 0x01) << 4) | (b[13] >> 4)) + 1;
    let samples = (((b[13] & 0x0f) as u64) << 32)
        | ((b[14] as u64) << 24)
        | ((b[15] as u64) << 16)
        | ((b[16] as u64) << 8)
        | (b[17] as u64);
    let md5 = &b[18..34];
    if md5.iter().all(|x| *x == 0) {
        return Ok(None);
    }
    let duration = if sample_rate > 0 { samples as f64 / sample_rate as f64 } else { 0.0 };
    Ok(Some(Info {
        key: format!("f:{}", hex16(md5)),
        exact: true,
        audio_start: 0,
        audio_len: 0,
        sample_rate,
        bits,
        channels,
        duration,
    }))
}

/// Untersucht eine Datei und liefert ihren Gruppierschluessel.
pub fn probe(path: &Path) -> std::io::Result<Info> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut head = [0u8; 10];
    let n = f.read(&mut head)?;
    let skip = id3v2_len(&head[..n]);

    // FLAC zuerst: dort gibt es die exakte Audio-Pruefsumme geschenkt
    if let Some(mut i) = flac_streaminfo(&mut f, skip)? {
        i.audio_start = skip;
        i.audio_len = len.saturating_sub(skip);
        return Ok(i);
    }

    let trail = trailer_len(&mut f, len).unwrap_or(0);
    let start = skip.min(len);
    let audio_len = len.saturating_sub(start).saturating_sub(trail);
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    Ok(Info {
        key: format!("s:{ext}:{audio_len}"),
        exact: false,
        audio_start: start,
        audio_len,
        sample_rate: 0,
        bits: 0,
        channels: 0,
        duration: 0.0,
    })
}

/// FNV-1a ueber den Audiobereich. Dient nur als Vorfilter – vor dem Loeschen
/// wird zusaetzlich byteweise verglichen.
pub fn hash_audio(path: &Path, start: u64, len: u64) -> std::io::Result<u64> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(start))?;
    let mut r = BufReader::with_capacity(128 * 1024, f.take(len));
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for x in &buf[..n] {
            h ^= *x as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    Ok(h)
}

/// Vergleicht zwei Audiobereiche Byte fuer Byte.
pub fn same_audio(a: &Path, a_start: u64, b: &Path, b_start: u64, len: u64) -> std::io::Result<bool> {
    let fa = File::open(a)?;
    let fb = File::open(b)?;
    let mut ra = BufReader::with_capacity(128 * 1024, fa);
    let mut rb = BufReader::with_capacity(128 * 1024, fb);
    ra.seek(SeekFrom::Start(a_start))?;
    rb.seek(SeekFrom::Start(b_start))?;
    let mut left = len;
    let (mut ba, mut bb) = ([0u8; 64 * 1024], [0u8; 64 * 1024]);
    while left > 0 {
        let want = left.min(ba.len() as u64) as usize;
        ra.read_exact(&mut ba[..want])?;
        rb.read_exact(&mut bb[..want])?;
        if ba[..want] != bb[..want] {
            return Ok(false);
        }
        left -= want as u64;
    }
    Ok(true)
}

#[cfg(test)]
/// Baut eine FLAC-Datei mit STREAMINFO (ohne echte Audio-Frames – zum Testen
/// des Parsers reicht das, denn gelesen werden nur die ersten 42 Bytes).
pub fn flac_bytes(md5: [u8; 16], sample_rate: u32, bits: u8, channels: u8, samples: u64, pad: usize) -> Vec<u8> {
    let mut v = b"fLaC".to_vec();
    v.extend_from_slice(&[0x00, 0x00, 0x00, 34]); // STREAMINFO, 34 Byte
    let mut b = [0u8; 34];
    b[10] = (sample_rate >> 12) as u8;
    b[11] = (sample_rate >> 4) as u8;
    b[12] = (((sample_rate & 0x0f) as u8) << 4) | ((channels - 1) << 1) | ((bits - 1) >> 4);
    b[13] = (((bits - 1) & 0x0f) << 4) | ((samples >> 32) as u8 & 0x0f);
    b[14] = (samples >> 24) as u8;
    b[15] = (samples >> 16) as u8;
    b[16] = (samples >> 8) as u8;
    b[17] = samples as u8;
    b[18..34].copy_from_slice(&md5);
    v.extend_from_slice(&b);
    v.extend(std::iter::repeat(b'A').take(pad));
    v
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("nd-audio-{}-{}", std::process::id(), name));
        let _ = std::fs::create_dir_all(d.parent().unwrap());
        d
    }
    fn write(name: &str, data: &[u8]) -> std::path::PathBuf {
        let p = tmp(name);
        File::create(&p).unwrap().write_all(data).unwrap();
        p
    }

    #[test]
    fn flac_streaminfo_wird_gelesen() {
        let md5 = [0x11u8; 16];
        let p = write("a.flac", &flac_bytes(md5, 44100, 16, 2, 441000, 100));
        let i = probe(&p).unwrap();
        assert!(i.exact, "FLAC-MD5 identifiziert das Audio eindeutig");
        assert_eq!(i.key, format!("f:{}", "11".repeat(16)));
        assert_eq!(i.sample_rate, 44100);
        assert_eq!(i.bits, 16);
        assert_eq!(i.channels, 2);
        assert!((i.duration - 10.0).abs() < 0.001, "441000 Samples bei 44,1 kHz = 10 s");
    }

    #[test]
    fn gleiches_audio_trotz_anderer_kompression() {
        let md5 = [0x22u8; 16];
        // gleiche Pruefsumme, unterschiedlich grosse Dateien
        let a = write("b1.flac", &flac_bytes(md5, 48000, 24, 2, 96000, 500));
        let b = write("b2.flac", &flac_bytes(md5, 48000, 24, 2, 96000, 9000));
        let (ia, ib) = (probe(&a).unwrap(), probe(&b).unwrap());
        assert_eq!(ia.key, ib.key, "gleiches Audio wird trotz anderer Dateigroesse erkannt");
        assert_eq!(ia.bits, 24);
    }

    #[test]
    fn unterschiedliches_audio_bleibt_getrennt() {
        let a = write("c1.flac", &flac_bytes([0x33; 16], 44100, 16, 2, 441000, 10));
        let b = write("c2.flac", &flac_bytes([0x44; 16], 44100, 16, 2, 441000, 10));
        assert_ne!(probe(&a).unwrap().key, probe(&b).unwrap().key);
    }

    #[test]
    fn flac_ohne_pruefsumme_faellt_auf_den_stream_zurueck() {
        let p = write("d.flac", &flac_bytes([0; 16], 44100, 16, 2, 441000, 10));
        let i = probe(&p).unwrap();
        assert!(!i.exact, "Nullsumme heisst 'unbekannt'");
        assert!(i.key.starts_with("s:flac:"));
    }

    #[test]
    fn id3_tags_zaehlen_nicht_zum_audio() {
        // ID3v2-Kopf (10 + 20 Byte), danach Audio, am Ende ID3v1 (128 Byte)
        let mut a = b"ID3\x04\x00\x00\x00\x00\x00\x14".to_vec();
        a.extend(std::iter::repeat(b'X').take(20));
        a.extend(std::iter::repeat(b'M').take(1000));
        a.extend(b"TAG".iter().copied());
        a.extend(std::iter::repeat(b'1').take(125));
        // dieselbe Musik, andere Tags: anderer ID3v2-Block, kein ID3v1
        let mut b = b"ID3\x04\x00\x00\x00\x00\x00\x28".to_vec();
        b.extend(std::iter::repeat(b'Y').take(40));
        b.extend(std::iter::repeat(b'M').take(1000));

        let pa = write("e1.mp3", &a);
        let pb = write("e2.mp3", &b);
        let (ia, ib) = (probe(&pa).unwrap(), probe(&pb).unwrap());
        assert_eq!(ia.audio_len, 1000);
        assert_eq!(ib.audio_len, 1000);
        assert_eq!(ia.key, ib.key, "gleiche Audio-Laenge trotz anderer Tags");
        assert_eq!(hash_audio(&pa, ia.audio_start, ia.audio_len).unwrap(),
                   hash_audio(&pb, ib.audio_start, ib.audio_len).unwrap());
        assert!(same_audio(&pa, ia.audio_start, &pb, ib.audio_start, 1000).unwrap(),
                "byteweiser Vergleich bestaetigt die Gleichheit");
    }

    #[test]
    fn unterschiedliches_audio_faellt_beim_byte_vergleich_auf() {
        let a = write("f1.mp3", &[b'M'; 500]);
        let mut d = vec![b'M'; 500];
        d[400] = b'Z';
        let b = write("f2.mp3", &d);
        let (ia, ib) = (probe(&a).unwrap(), probe(&b).unwrap());
        assert_eq!(ia.key, ib.key, "gleiche Laenge, also gleicher Vorfilter");
        assert_ne!(hash_audio(&a, 0, 500).unwrap(), hash_audio(&b, 0, 500).unwrap());
        assert!(!same_audio(&a, ia.audio_start, &b, ib.audio_start, 500).unwrap());
    }
}
