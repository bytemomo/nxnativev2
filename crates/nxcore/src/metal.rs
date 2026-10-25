//! Apple's Metal Developer Tools for Windows (the user's download): the shader compiler of the pipeline.
//! Windows runs them natively; Linux runs them with wine, in a wine prefix of our own under the data folder.
//! The nxnative script reaches them through its `xcrun` (dm/linux/bin/xcrun: DM_METAL_WIN, WINEPREFIX).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::store::Store;

/// Where the installer puts the tools: `<prefix>/drive_c/Program Files/Metal Developer Tools` (Linux),
/// `<metal dir>` itself (Windows, installed with /D=)
pub fn tools_dir(store: &Store) -> PathBuf {
    if cfg!(windows) { store.metal_dir() } else { wine_prefix(store).join("drive_c/Program Files/Metal Developer Tools") }
}

pub fn wine_prefix(store: &Store) -> PathBuf {
    store.metal_dir().join("wine")
}

pub fn installed(store: &Store) -> bool {
    tools_dir(store).join("bin/metal.exe").is_file()
}

/// The environment the pipeline needs to find the tools
pub fn env(store: &Store) -> Vec<(String, String)> {
    let mut v = vec![("DM_METAL_WIN".to_string(), tools_dir(store).to_string_lossy().into_owned())];
    if !cfg!(windows) {
        v.push(("WINEPREFIX".into(), wine_prefix(store).to_string_lossy().into_owned()));
        v.push(("WINEDEBUG".into(), "-all".into()));
    }
    v
}

fn wine_command(store: &Store, exe: &Path) -> Command {
    let mut c = Command::new("wine");
    c.arg(exe)
        .env("WINEPREFIX", wine_prefix(store))
        .env("WINEDEBUG", "-all")
        // no Mono / Gecko prompts when the prefix is created
        .env("WINEDLLOVERRIDES", "mscoree,mshtml=");
    c
}

/// Installs the tools from the user's installer (`Metal Developer Tools-Windows.exe`) and returns the compiler's version line
pub fn install(store: &Store, installer: &Path) -> Result<String> {
    ensure!(installer.is_file(), "no installer at {}", installer.display());
    std::fs::create_dir_all(store.metal_dir())?;
    let status = if cfg!(windows) {
        // /D must be last and unquoted (NSIS)
        Command::new(installer).arg("/S").arg(format!("/D={}", store.metal_dir().display())).status()
    } else {
        which("wine").context("wine is needed to run Apple's Metal compiler on Linux (install the wine package)")?;
        wine_command(store, installer).arg("/S").status()
    }
    .context("start the installer")?;
    ensure!(status.success(), "the Metal tools installer failed ({status})");
    ensure!(installed(store), "the installer finished but no metal.exe is in {}", tools_dir(store).display());
    align_for_wine(store)?;
    version(store)
}

/// The compiler's version line ("Apple metal version 32023.850 ...")
pub fn version(store: &Store) -> Result<String> {
    let bin = tools_dir(store).join("bin");
    // 6.x: the compiler itself is under metal/<version>/bin; bin/metal.exe only launches it (4 s per call under wine)
    let inner = std::fs::read_dir(tools_dir(store).join("metal"))
        .ok()
        .and_then(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path().join("bin/metal.exe")).filter(|p| p.is_file()).max());
    let exe = inner.unwrap_or_else(|| bin.join("metal.exe"));
    let out = if cfg!(windows) { Command::new(&exe).arg("--version").output() } else { wine_command(store, &exe).arg("--version").output() }?;
    let text = String::from_utf8_lossy(&out.stdout);
    match text.lines().next() {
        Some(l) if l.contains("metal") => Ok(l.trim().to_string()),
        _ => bail!("the Metal compiler does not run: {}", String::from_utf8_lossy(&out.stderr).trim()),
    }
}

/// Linux: the compiler's executables with their sections page-aligned in the file (FileAlignment 0x1000), done once.
/// Apple ships them with FileAlignment 0x200, which wine cannot map: every process then reads the whole image into private memory
/// (metal.exe is 119 MB: ~35,000 page faults and a third of a compile's CPU time, most of it in the kernel). Only file offsets move;
/// the code, the data and so the outputs are the same (checked byte for byte on 64 shaders, AIR and metallib). Each rewritten file must
/// still answer --version under wine (exit status 0, some output), else the original stays.
pub fn align_for_wine(store: &Store) -> Result<()> {
    if cfg!(windows) {
        return Ok(());
    }
    let Ok(rd) = std::fs::read_dir(tools_dir(store).join("metal")) else { return Ok(()) };
    for dir in rd.filter_map(|e| e.ok()).map(|e| e.path().join("bin")) {
        for name in ["metal.exe", "metallib.exe"] {
            let exe = dir.join(name);
            let Ok(data) = std::fs::read(&exe) else { continue };
            let Some(aligned) = pe_page_aligned(&data)? else { continue };
            // under its own name in a side folder: the tools pick their behaviour from their file name (metallib.exe.aligned "cannot detect flavor")
            let side = dir.join(".aligned");
            std::fs::create_dir_all(&side)?;
            let tmp = side.join(name);
            std::fs::write(&tmp, &aligned)?;
            let ok = wine_command(store, &tmp).arg("--version").output().map(|o| o.status.success() && !o.stdout.is_empty()).unwrap_or(false);
            if !ok {
                let _ = std::fs::remove_dir_all(&side);
                tracing::warn!(exe = %exe.display(), "Metal tools: the page-aligned copy does not run under wine; the original stays");
                continue;
            }
            // a rename: compiles already running keep the file they opened
            std::fs::rename(&tmp, &exe)?;
            let _ = std::fs::remove_dir(&side);
            tracing::info!(exe = %exe.display(), "Metal tools: sections page-aligned for wine");
        }
    }
    Ok(())
}

/// A PE32+ image with each section's file data at a page-aligned offset; None when it is so already (or not a PE32+ image)
fn pe_page_aligned(b: &[u8]) -> Result<Option<Vec<u8>>> {
    const PAGE: u32 = 0x1000;
    let u16_at = |o: usize| -> Result<u32> { Ok(u16::from_le_bytes(b.get(o..o + 2).context("truncated PE")?.try_into()?) as u32) };
    let u32_at = |o: usize| -> Result<u32> { Ok(u32::from_le_bytes(b.get(o..o + 4).context("truncated PE")?.try_into()?)) };
    let put = |v: &mut Vec<u8>, o: usize, x: u32| v[o..o + 4].copy_from_slice(&x.to_le_bytes());
    let align = |n: u32| n.div_ceil(PAGE) * PAGE;
    if b.len() < 0x40 || &b[..2] != b"MZ" {
        return Ok(None);
    }
    let pe = u32_at(0x3c)? as usize;
    if b.get(pe..pe + 4) != Some(b"PE\0\0".as_slice()) {
        return Ok(None);
    }
    let nsec = u16_at(pe + 6)? as usize;
    let opt = pe + 24;
    let sec0 = opt + u16_at(pe + 20)? as usize;
    if u16_at(opt)? != 0x20b || u32_at(opt + 36)? >= PAGE {
        return Ok(None);
    }
    let hdr = u32_at(opt + 60)?;
    // (header offset, raw size, raw offset) per section with file data, in file order
    let mut secs: Vec<(usize, u32, u32)> = (0..nsec).map(|i| sec0 + 40 * i).map(|o| Ok((o, u32_at(o + 16)?, u32_at(o + 20)?))).collect::<Result<_>>()?;
    secs.retain(|s| s.1 > 0);
    secs.sort_by_key(|s| s.2);
    let end = secs.iter().map(|s| s.2 + s.1).max().unwrap_or(hdr) as usize;
    let mut out = b[..hdr as usize].to_vec();
    out.resize(align(hdr) as usize, 0);
    let mut moves = vec![];
    for &(o, size, raw) in &secs {
        let new = out.len() as u32;
        out.extend_from_slice(b.get(raw as usize..(raw + size) as usize).context("section past the end of the file")?);
        out.resize(align(out.len() as u32) as usize, 0);
        put(&mut out, o + 16, align(size));
        put(&mut out, o + 20, new);
        moves.push((raw, size, new));
    }
    let tail = out.len() as u32;
    out.extend_from_slice(&b[end.min(b.len())..]);
    let remap = |off: u32| moves.iter().find(|m| off >= m.0 && off < m.0 + m.1).map(|m| m.2 + off - m.0).or((off as usize >= end).then(|| tail + off - end as u32));
    put(&mut out, opt + 36, PAGE); // FileAlignment
    put(&mut out, opt + 60, align(hdr)); // SizeOfHeaders
    put(&mut out, opt + 64, 0); // CheckSum: not checked for executables
    let ndirs = u32_at(opt + 108)? as usize;
    let dirs = opt + 112;
    if ndirs > 4 {
        // the certificate table is a file offset, and the signature no longer matches the file: dropped
        put(&mut out, dirs + 32, 0);
        put(&mut out, dirs + 36, 0);
    }
    if ndirs > 6 {
        // the debug directory's entries point at their data by file offset
        let (dva, dsz) = (u32_at(dirs + 48)?, u32_at(dirs + 52)?);
        let at_va = |va: u32| {
            (0..nsec).map(|i| sec0 + 40 * i).find_map(|o| {
                let (vsz, sva, rsz, raw) = (u32_at(o + 8).ok()?, u32_at(o + 12).ok()?, u32_at(o + 16).ok()?, u32_at(o + 20).ok()?);
                (va >= sva && va < sva + vsz.max(rsz)).then(|| raw + va - sva)
            })
        };
        if let Some(old) = (dva != 0).then(|| at_va(dva)).flatten() {
            let new = remap(old).context("debug directory outside the sections")?;
            for k in 0..(dsz / 28) as usize {
                let p = u32_at(old as usize + 28 * k + 24)?;
                if p != 0 {
                    put(&mut out, new as usize + 28 * k + 24, remap(p).unwrap_or(0));
                }
            }
        }
    }
    Ok(Some(out))
}

pub fn which(tool: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &["exe", "cmd", "bat"] } else { &[""] };
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|d| {
        exts.iter().map(|e| if e.is_empty() { d.join(tool) } else { d.join(format!("{tool}.{e}")) }).find(|p| p.is_file())
    })
}

#[cfg(test)]
mod tests {
    /// NXNATIVE_TEST_ALIGN=<folder with Apple's metal.exe and metallib.exe>: align_for_wine on a scratch data folder (its own wine prefix) aligns both, and they still run
    #[test]
    fn align_for_wine_both_tools() {
        let Ok(src) = std::env::var("NXNATIVE_TEST_ALIGN") else { return };
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/align-test");
        let _ = std::fs::remove_dir_all(&root);
        let store = crate::store::Store::new(root.clone()).unwrap();
        let bin = super::tools_dir(&store).join("metal/32023/bin");
        std::fs::create_dir_all(&bin).unwrap();
        for n in ["metal.exe", "metallib.exe"] {
            std::fs::copy(std::path::Path::new(&src).join(n), bin.join(n)).unwrap();
        }
        super::align_for_wine(&store).unwrap();
        for n in ["metal.exe", "metallib.exe"] {
            assert!(super::pe_page_aligned(&std::fs::read(bin.join(n)).unwrap()).unwrap().is_none(), "{n} not aligned");
        }
        assert!(!bin.join(".aligned").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// NXNATIVE_TEST_PE=<original.exe>:<page-aligned reference.exe>[,...]: the realignment gives the reference byte for byte, and leaves an aligned file alone
    #[test]
    fn pe_page_aligned_matches() {
        let Ok(v) = std::env::var("NXNATIVE_TEST_PE") else { return };
        for pair in v.split(',') {
            let (orig, want) = pair.split_once(':').unwrap();
            let got = super::pe_page_aligned(&std::fs::read(orig).unwrap()).unwrap().expect("not aligned yet");
            assert!(got == std::fs::read(want).unwrap(), "{orig}: differs from {want}");
            assert!(super::pe_page_aligned(&got).unwrap().is_none(), "{orig}: aligned twice");
        }
    }
}
