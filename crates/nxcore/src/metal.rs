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

pub fn which(tool: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &["exe", "cmd", "bat"] } else { &[""] };
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|d| {
        exts.iter().map(|e| if e.is_empty() { d.join(tool) } else { d.join(format!("{tool}.{e}")) }).find(|p| p.is_file())
    })
}
