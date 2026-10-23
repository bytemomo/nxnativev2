//! Recognising Switch game files: title ID, kind (base game / update / DLC) and version, as the nxnative script does
//! (`scripts/build_ipa_from_nsp.py` title_of / version_of / scan_nsps).

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, ensure};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Base,
    Update,
    Dlc,
}

#[derive(Debug, Clone)]
pub struct GameFile {
    /// The file's own title ID (base …000, update …800, DLC base + 0x1000 + index)
    pub title_id: u64,
    /// The base game's title ID: the game this file belongs to
    pub app_id: u64,
    pub kind: Kind,
    /// `[v<N>]` in the file name
    pub version: Option<u32>,
}

/// The file names inside an NSP (a PFS0 archive); empty for anything else (an XCI)
pub fn pfs0_names(path: &Path) -> Result<Vec<String>> {
    let mut f = File::open(path)?;
    let mut h = [0u8; 16];
    if f.read_exact(&mut h).is_err() || &h[0..4] != b"PFS0" {
        return Ok(vec![]);
    }
    let n = u32::from_le_bytes(h[4..8].try_into().unwrap()) as usize;
    let st = u32::from_le_bytes(h[8..12].try_into().unwrap()) as usize;
    ensure!(n < 1 << 16 && st < 1 << 24, "implausible PFS0 header");
    let mut ent = vec![0u8; n * 0x18];
    let mut strs = vec![0u8; st];
    f.read_exact(&mut ent)?;
    f.read_exact(&mut strs)?;
    Ok((0..n)
        .map(|i| {
            let o = u32::from_le_bytes(ent[i * 0x18 + 0x10..i * 0x18 + 0x14].try_into().unwrap()) as usize;
            let end = strs[o..].iter().position(|&b| b == 0).map_or(strs.len(), |p| o + p);
            String::from_utf8_lossy(&strs[o..end]).into_owned()
        })
        .collect())
}

fn bracket_hex16(name: &str) -> Option<u64> {
    name.match_indices('[').find_map(|(i, _)| {
        let tag = name.get(i + 1..i + 17)?;
        (name.get(i + 17..i + 18) == Some("]")).then(|| u64::from_str_radix(tag, 16).ok())?
    })
}

fn bracket_version(name: &str) -> Option<u32> {
    let i = name.find("[v")?;
    let rest = &name[i + 2..];
    rest[..rest.find(']')?].parse().ok()
}

/// The title ID: the ticket's name first (the first 16 hex digits of the rights ID; a file name's tag can be wrong), else `[<16 hex>]` in the file name
pub fn title_id(path: &Path) -> Result<Option<u64>> {
    for n in pfs0_names(path)? {
        if n.len() == 36 && n.ends_with(".tik") && n[..32].chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(Some(u64::from_str_radix(&n[..16], 16)?));
        }
    }
    Ok(bracket_hex16(&path.file_name().unwrap_or_default().to_string_lossy()))
}

pub fn identify(path: &Path) -> Result<GameFile> {
    let tid = title_id(path)?.with_context(|| format!("{}: no title ID (no ticket, no [title ID] in the name)", path.display()))?;
    let low = tid & 0xFFF;
    let (kind, app_id) = match low {
        0 => (Kind::Base, tid),
        0x800 => (Kind::Update, tid & !0xFFF),
        _ => (Kind::Dlc, (tid & !0xFFF) - 0x1000),
    };
    let version = bracket_version(&path.file_name().unwrap_or_default().to_string_lossy());
    Ok(GameFile { title_id: tid, app_id, kind, version })
}

pub fn is_game_file(path: &Path) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(), Some("nsp" | "xci"))
}

/// A readable name from a file name: "Hollow Knight Silksong [010013C00E930000][v0].nsp" → "Hollow Knight Silksong"
pub fn name_from_file(path: &Path) -> String {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    stem.split('[').next().unwrap_or(&stem).trim().to_string()
}
