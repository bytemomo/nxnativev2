//! The font the iOS app gets as the Switch's standard shared font (build_ipa: STD_FONT). The host gives one font for the standard, Chinese and
//! Korean shared fonts (dm/host/hle_sdk16.c), and games look up characters in it at startup: The Legend of Zelda: Echoes of Wisdom checks for
//! U+25A1 (□) and crashes on a null glyph without it. The bundle's DejaVu Sans has it; a font of this computer that covers more languages
//! (Japanese, Chinese, Korean: Arial Unicode, a pan-CJK TrueType font) is taken instead. Only the tables this needs are read (a system can hold
//! gigabytes of fonts): the table directory, OS/2, post and cmap.
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Characters a candidate must map: printable ASCII and U+25A1
const REQUIRED: &[(u32, u32)] = &[(0x20, 0x7e), (0x25a1, 0x25a1)];
/// Coverage of each block counts for its weight: the Latin, Greek and Cyrillic text of European languages, punctuation and shapes,
/// then the scripts of the Chinese, Japanese and Korean shared fonts
const BLOCKS: &[(u32, u32, f64)] = &[
    (0x00a0, 0x00ff, 1.0),  // Latin-1
    (0x0100, 0x017f, 1.0),  // Latin Extended-A
    (0x0370, 0x03ff, 1.0),  // Greek
    (0x0400, 0x04ff, 1.0),  // Cyrillic
    (0x2000, 0x206f, 1.0),  // General Punctuation
    (0x25a0, 0x25ff, 1.0),  // Geometric Shapes
    (0x3040, 0x30ff, 2.0),  // Hiragana, Katakana
    (0x4e00, 0x9fff, 2.0),  // CJK Unified Ideographs
    (0xac00, 0xd7a3, 2.0),  // Hangul Syllables
];
/// A font covering less of Latin-1 than this cannot show European languages' accents: never taken
const MIN_LATIN1: f64 = 0.9;
/// Larger files are skipped (Arial Unicode is 23 MB); the host maps the font into memory
const MAX_SIZE: u64 = 64 << 20;

#[derive(Debug, Clone)]
pub struct Choice {
    pub path: PathBuf,
    pub name: String,
    pub score: f64,
}

/// The standard font: the best qualifying font of this computer, else `bundled` (None when that does not qualify either)
pub fn standard(bundled: &Path) -> Option<Choice> {
    let mine = rate(bundled);
    let best = system_fonts().into_iter().filter_map(|p| rate(&p)).max_by(|a, b| a.score.total_cmp(&b.score));
    match (best, mine) {
        (Some(b), Some(m)) if b.score > m.score + 0.01 => Some(b),
        (b, m) => m.or(b),
    }
}

/// TrueType fonts in the system's and the user's font folders
fn system_fonts() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = vec![];
    if cfg!(windows) {
        dirs.push(PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts"));
        if let Some(l) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(l).join("Microsoft\\Windows\\Fonts"));
        }
    } else {
        dirs.extend(["/usr/share/fonts", "/usr/local/share/fonts"].map(PathBuf::from));
        if let Some(h) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(&h).join(".local/share/fonts"));
            dirs.push(PathBuf::from(&h).join(".fonts"));
        }
    }
    let mut out = vec![];
    let mut todo: Vec<(PathBuf, u32)> = dirs.into_iter().map(|d| (d, 0)).collect();
    while let Some((d, depth)) = todo.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() && depth < 6 {
                todo.push((p, depth + 1));
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("ttf")) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// A qualifying font's score; None for one that is not a plain upright TrueType font mapping REQUIRED
fn rate(p: &Path) -> Option<Choice> {
    let mut f = File::open(p).ok()?;
    if f.metadata().ok()?.len() > MAX_SIZE {
        return None;
    }
    let mut head = [0u8; 12];
    f.read_exact(&mut head).ok()?;
    // TrueType outlines only (not CFF 'OTTO', not a 'ttcf' collection): what the Switch's font engine reads
    if !matches!(&head[..4], [0, 1, 0, 0] | b"true") {
        return None;
    }
    let n = be16(&head, 4) as usize;
    let mut dir = vec![0u8; n * 16];
    f.read_exact(&mut dir).ok()?;
    let table = |tag: &[u8; 4]| dir.chunks_exact(16).find(|r| &r[..4] == tag).map(|r| (be32(r, 8) as u64, be32(r, 12) as usize));
    // a variable font's glyphs need its variation tables, which the engine may not apply
    if table(b"glyf").is_none() || table(b"loca").is_none() || table(b"fvar").is_some() {
        return None;
    }
    let os2 = read(&mut f, table(b"OS/2")?, 64)?;
    let (weight, width, selection) = (be16(&os2, 4), be16(&os2, 6), be16(&os2, 62));
    // upright, regular weight, normal width: italic (fsSelection bit 0), bold (5), oblique (9) and condensed or expanded faces are skipped
    if !(350..=500).contains(&weight) || width != 5 || selection & 0x221 != 0 {
        return None;
    }
    // monospaced: post's isFixedPitch, or PANOSE proportion 9 (Latin text faces)
    let post = read(&mut f, table(b"post")?, 16)?;
    if be32(&post, 12) != 0 || (os2[32] == 2 && os2[35] == 9) {
        return None;
    }
    let cmap = Cmap4::read(&mut f, table(b"cmap")?)?;
    if !REQUIRED.iter().all(|&(a, b)| (a..=b).all(|c| cmap.maps(c))) {
        return None;
    }
    let cover = |a: u32, b: u32| (a..=b).filter(|&c| cmap.maps(c)).count() as f64 / (b - a + 1) as f64;
    if cover(0xa0, 0xff) < MIN_LATIN1 {
        return None;
    }
    let score = BLOCKS.iter().map(|&(a, b, w)| w * cover(a, b)).sum();
    let name = p.file_name()?.to_string_lossy().into_owned();
    Some(Choice { path: p.to_path_buf(), name, score })
}

fn read(f: &mut File, (off, len): (u64, usize), min: usize) -> Option<Vec<u8>> {
    if len < min || len > 16 << 20 {
        return None;
    }
    let mut b = vec![0u8; len];
    f.seek(SeekFrom::Start(off)).ok()?;
    f.read_exact(&mut b).ok()?;
    Some(b)
}

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}
fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// The Unicode BMP subtable (format 4: Windows Unicode, else a Unicode platform one), as segments
struct Cmap4 {
    sub: Vec<u8>,
    segs: usize,
}

impl Cmap4 {
    fn read(f: &mut File, t: (u64, usize)) -> Option<Self> {
        let cmap = read(f, t, 4)?;
        let n = be16(&cmap, 2) as usize;
        let mut best = None;
        for i in 0..n {
            let r = 4 + i * 8;
            if r + 8 > cmap.len() {
                return None;
            }
            let (plat, enc, off) = (be16(&cmap, r), be16(&cmap, r + 2), be32(&cmap, r + 4) as usize);
            if off + 4 > cmap.len() || be16(&cmap, off) != 4 {
                continue;
            }
            let rank = match (plat, enc) {
                (3, 1) => 2,
                (0, _) => 1,
                _ => continue,
            };
            if best.is_none_or(|(r, _)| rank > r) {
                best = Some((rank, off));
            }
        }
        let off = best?.1;
        let len = (be16(&cmap, off + 2) as usize).min(cmap.len() - off);
        let sub = cmap[off..off + len].to_vec();
        let segs = be16(&sub, 6) as usize / 2;
        (sub.len() >= 16 + segs * 8).then_some(Cmap4 { sub, segs })
    }

    /// Whether character `c` has a glyph other than .notdef
    fn maps(&self, c: u32) -> bool {
        if c > 0xffff {
            return false;
        }
        let s = &self.sub;
        let (end, start, delta, range) = (14, 16 + self.segs * 2, 16 + self.segs * 4, 16 + self.segs * 6);
        // the segments are sorted by end code
        let (mut lo, mut hi) = (0, self.segs);
        while lo < hi {
            let m = (lo + hi) / 2;
            if (be16(s, end + m * 2) as u32) < c { lo = m + 1 } else { hi = m }
        }
        if lo == self.segs || (be16(s, start + lo * 2) as u32) > c {
            return false;
        }
        let ro = be16(s, range + lo * 2) as usize;
        let g = if ro == 0 {
            (c as u16).wrapping_add(be16(s, delta + lo * 2))
        } else {
            let at = range + lo * 2 + ro + (c - be16(s, start + lo * 2) as u32) as usize * 2;
            if at + 2 > s.len() {
                return false;
            }
            match be16(s, at) {
                0 => 0,
                g => g.wrapping_add(be16(s, delta + lo * 2)),
            }
        };
        g != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NXNATIVE_TEST_FONTS=1: every qualifying font of this computer with its score, and the one taken over `NXNATIVE_TEST_FONTS_BUNDLED` (read only)
    #[test]
    fn list_system_fonts() {
        if std::env::var_os("NXNATIVE_TEST_FONTS").is_none() {
            return;
        }
        let mut v: Vec<Choice> = system_fonts().iter().filter_map(|p| rate(p)).collect();
        v.sort_by(|a, b| b.score.total_cmp(&a.score));
        for c in &v {
            println!("{:6.3}  {}", c.score, c.path.display());
        }
        if let Some(b) = std::env::var_os("NXNATIVE_TEST_FONTS_BUNDLED") {
            println!("bundled: {:?}\ntaken: {:?}", rate(Path::new(&b)), standard(Path::new(&b)));
        }
    }
}
