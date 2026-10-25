//! What an iOS app's IPA says about itself: its bundle ID, name, version and icon (from `Payload/<name>.app/Info.plist`), for an IPA
//! added as is (an app to sideload, not a game built here).

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Info {
    pub bundle_id: String,
    pub name: String,
    pub version: Option<String>,
    /// The largest app icon, as a PNG any decoder reads (Xcode's crushed PNGs are converted)
    pub icon: Option<Vec<u8>>,
}

pub fn read(ipa: &Path) -> Result<Info> {
    let mut zip = zip::ZipArchive::new(File::open(ipa).with_context(|| format!("open {}", ipa.display()))?).context("not an IPA (a zip archive)")?;
    let names: Vec<String> = (0..zip.len()).filter_map(|i| zip.name_for_index(i).map(str::to_string)).collect();
    let prefix = names
        .iter()
        .find_map(|n| {
            let rest = n.strip_prefix("Payload/")?;
            let end = rest.find(".app/")?;
            Some(format!("Payload/{}.app/", &rest[..end]))
        })
        .context("no Payload/<name>.app in the IPA")?;
    let info: plist::Dictionary = {
        let mut f = zip.by_name(&format!("{prefix}Info.plist")).context("no Info.plist in the app")?;
        let mut b = vec![];
        f.read_to_end(&mut b)?;
        plist::from_bytes(&b).context("read the app's Info.plist")?
    };
    let get = |k: &str| info.get(k).and_then(|v| v.as_string()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let bundle_id = get("CFBundleIdentifier").context("no CFBundleIdentifier in the app's Info.plist")?;
    let name = get("CFBundleDisplayName").or_else(|| get("CFBundleName")).unwrap_or_else(|| bundle_id.clone());
    let version = get("CFBundleShortVersionString").or_else(|| get("CFBundleVersion"));

    // the icon files the plist names (without their @2x~ipad.png endings), else the usual names; the largest is the sharpest
    let mut stems: Vec<String> = vec![];
    for key in ["CFBundleIcons~ipad", "CFBundleIcons"] {
        let files = info.get(key).and_then(|v| v.as_dictionary()).and_then(|d| d.get("CFBundlePrimaryIcon")).and_then(|v| v.as_dictionary()).and_then(|d| d.get("CFBundleIconFiles"));
        stems.extend(files.and_then(|v| v.as_array()).into_iter().flatten().filter_map(|v| v.as_string()).map(str::to_string));
    }
    stems.extend(info.get("CFBundleIconFiles").and_then(|v| v.as_array()).into_iter().flatten().filter_map(|v| v.as_string()).map(str::to_string));
    stems.extend(get("CFBundleIconFile"));
    stems.extend(["AppIcon", "Icon"].map(String::from));
    let stems: Vec<String> = stems.into_iter().map(|s| s.trim_end_matches(".png").to_lowercase()).filter(|s| !s.is_empty()).collect();
    let mut best: Option<(u64, String)> = None;
    for n in &names {
        let Some(file) = n.strip_prefix(&prefix).filter(|f| !f.contains('/')) else { continue };
        let lower = file.to_lowercase();
        if !lower.ends_with(".png") || !stems.iter().any(|s| lower.starts_with(s)) {
            continue;
        }
        let size = zip.by_name(n).map(|e| e.size()).unwrap_or(0);
        if best.as_ref().is_none_or(|(s, _)| size > *s) {
            best = Some((size, n.clone()));
        }
    }
    let icon = best.and_then(|(_, n)| {
        let mut b = vec![];
        zip.by_name(&n).ok()?.read_to_end(&mut b).ok()?;
        plain_png(&b)
    });
    Ok(Info { bundle_id, name, version, icon })
}

/// The PNG as is, or Xcode's crushed one (a CgBI chunk first, raw deflate, BGRA with premultiplied alpha) as a standard PNG.
/// None when it cannot be read.
pub fn plain_png(b: &[u8]) -> Option<Vec<u8>> {
    const SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if !b.starts_with(SIG) {
        return None;
    }
    let mut chunks = vec![];
    let mut at = SIG.len();
    while at + 12 <= b.len() {
        let len = u32::from_be_bytes(b[at..at + 4].try_into().ok()?) as usize;
        let kind: [u8; 4] = b[at + 4..at + 8].try_into().ok()?;
        let data = b.get(at + 8..at + 8 + len)?;
        chunks.push((kind, data));
        at += 12 + len;
    }
    if chunks.first().map(|c| &c.0) != Some(b"CgBI") {
        return Some(b.to_vec());
    }
    let ihdr = chunks.iter().find(|c| &c.0 == b"IHDR")?.1.get(..13)?;
    let (w, h) = (u32::from_be_bytes(ihdr[0..4].try_into().ok()?) as usize, u32::from_be_bytes(ihdr[4..8].try_into().ok()?) as usize);
    // 8-bit RGBA, not interlaced: what Xcode writes
    if ihdr[8] != 8 || ihdr[9] != 6 || ihdr[12] != 0 {
        return None;
    }
    let deflated: Vec<u8> = chunks.iter().filter(|c| &c.0 == b"IDAT").flat_map(|c| c.1.iter().copied()).collect();
    let mut raw = vec![];
    flate2::read::DeflateDecoder::new(&deflated[..]).read_to_end(&mut raw).ok()?;
    let stride = w * 4;
    if raw.len() < h * (stride + 1) {
        return None;
    }
    // undo the scanline filters, then BGRA premultiplied → RGBA
    let mut px = vec![0u8; h * stride];
    for y in 0..h {
        let filter = raw[y * (stride + 1)];
        let line = &raw[y * (stride + 1) + 1..(y + 1) * (stride + 1)];
        for x in 0..stride {
            let a = if x >= 4 { px[y * stride + x - 4] as i16 } else { 0 };
            let up = if y > 0 { px[(y - 1) * stride + x] as i16 } else { 0 };
            let c = if x >= 4 && y > 0 { px[(y - 1) * stride + x - 4] as i16 } else { 0 };
            let pred = match filter {
                0 => 0,
                1 => a,
                2 => up,
                3 => (a + up) / 2,
                4 => {
                    let p = a + up - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - up).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc { a } else if pb <= pc { up } else { c }
                }
                _ => return None,
            };
            px[y * stride + x] = line[x].wrapping_add(pred as u8);
        }
    }
    for p in px.chunks_exact_mut(4) {
        p.swap(0, 2);
        let a = p[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let mut filtered = Vec::with_capacity(h * (stride + 1));
    for row in px.chunks_exact(stride) {
        filtered.push(0);
        filtered.extend_from_slice(row);
    }
    let mut z = flate2::write::ZlibEncoder::new(vec![], flate2::Compression::default());
    z.write_all(&filtered).ok()?;
    let idat = z.finish().ok()?;
    let mut out = SIG.to_vec();
    for (kind, data) in [(b"IHDR", ihdr), (b"IDAT", &idat[..]), (b"IEND", &[][..])] {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut crc = flate2::Crc::new();
        crc.update(kind);
        crc.update(data);
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc.sum().to_be_bytes());
    }
    Some(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// A 2×1 icon as Xcode crushes it: CgBI first, BGRA premultiplied, raw deflate
    fn crushed() -> Vec<u8> {
        let rows = [0u8, 0, 0, 255, 255, /* half-transparent red, premultiplied */ 0, 0, 128, 128];
        let mut d = flate2::write::DeflateEncoder::new(vec![], flate2::Compression::default());
        d.write_all(&rows).unwrap();
        let idat = d.finish().unwrap();
        let mut ihdr = vec![];
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, data) in [(&b"CgBI"[..], &[0x50, 0, 0x20, 2][..]), (b"IHDR", &ihdr), (b"IDAT", &idat), (b"IEND", &[])] {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            out.extend_from_slice(&[0; 4]);
        }
        out
    }

    pub(crate) fn sample_ipa(path: &Path, bundle_id: &str) {
        let mut z = zip::ZipWriter::new(File::create(path).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        let mut info = plist::Dictionary::new();
        info.insert("CFBundleIdentifier".into(), bundle_id.into());
        info.insert("CFBundleDisplayName".into(), "Sample".into());
        info.insert("CFBundleShortVersionString".into(), "1.2".into());
        let mut b = vec![];
        plist::to_writer_binary(&mut b, &plist::Value::Dictionary(info)).unwrap();
        z.start_file("Payload/Sample.app/Info.plist", o).unwrap();
        z.write_all(&b).unwrap();
        z.start_file("Payload/Sample.app/AppIcon60x60@2x.png", o).unwrap();
        z.write_all(&crushed()).unwrap();
        z.start_file("Payload/Sample.app/Sample", o).unwrap();
        z.write_all(b"binary").unwrap();
        z.finish().unwrap();
    }

    #[test]
    fn reads_an_ipa_and_uncrushes_its_icon() {
        let dir = std::env::temp_dir().join(format!("nxcore-ipa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("Sample.ipa");
        sample_ipa(&p, "com.example.sample");
        let i = read(&p).unwrap();
        assert_eq!((i.bundle_id.as_str(), i.name.as_str(), i.version.as_deref()), ("com.example.sample", "Sample", Some("1.2")));
        let png = i.icon.unwrap();
        // the standard PNG: IHDR then a zlib IDAT holding RGBA, alpha divided out
        assert_eq!(&png[12..16], b"IHDR");
        let len = u32::from_be_bytes(png[33..37].try_into().unwrap()) as usize;
        assert_eq!(&png[37..41], b"IDAT");
        let mut raw = vec![];
        flate2::read::ZlibDecoder::new(&png[41..41 + len]).read_to_end(&mut raw).unwrap();
        assert_eq!(raw, [0, 255, 0, 0, 255, 255, 0, 0, 128]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A real IPA (NXCORE_TEST_IPA): what it says, and its icon written beside the system's temp folder to look at
    #[test]
    #[ignore]
    fn real_ipa() {
        let p = std::env::var("NXCORE_TEST_IPA").expect("NXCORE_TEST_IPA");
        let i = read(Path::new(&p)).unwrap();
        println!("{} {} {:?} icon {:?} bytes", i.bundle_id, i.name, i.version, i.icon.as_ref().map(Vec::len));
        if let Some(png) = i.icon {
            std::fs::write(std::env::temp_dir().join("nxcore-ipa-icon.png"), png).unwrap();
        }
    }
}
