//! What the packaged app carries: a zstd-compressed tar appended to its own executable (`cargo xtask bundle` writes it), unpacked once into
//! `<data>/bundle/<hash>/`. Bundles also come as updates (update.rs: a release's `bundle-<platform>.tar.zst`), unpacked the same way. Each carries
//! its version in `bundle.json`; the active bundle is the newest complete one, so a slim client (an update of the app alone, with no payload)
//! uses the bundle it finds, and an app whose own payload is older than the installed one does not unpack it.
//!
//! ```text
//! bundle.json               version (a release number that only grows), platform, nxnative revision
//! nxnative/                 the pinned nxnative revision with the platform's patches applied
//! python/                   portable CPython with the pipeline's modules
//! dotnet/                   the .NET runtime (the shader translator and nx_extract run on it)
//! prebuilt/nx_extract/      nx_extract, compiled ahead of time
//! prebuilt/swift_DmIOS.o    the iOS app's Swift sources, compiled ahead of time (an arm64 iOS object: the same on every computer)
//! bin/                      spirv-cross (Windows: also xcrun.exe and dotnet-dll-launcher.exe, which the pipeline runs by name)
//! llvm/bin, llvm/lib/clang  clang and lld (ld64.lld) with clang's headers
//! ```
//!
//! Layout of the executable: `<program><payload: tar.zst><trailer>`, the trailer `<payload length u64 LE><version u64 LE><hash 16 ASCII><magic 8>`
//! (MAGIC2; an older app's trailer has no version: `<length><hash><MAGIC>`).

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::store::Store;

pub const MAGIC: &[u8; 8] = b"NXNV2PL1";
pub const MAGIC2: &[u8; 8] = b"NXNV2PL2";
pub const TRAILER: u64 = 32;
pub const TRAILER2: u64 = 40;
const COMPLETE: &str = ".nxnativev2-complete";
pub const INFO: &str = "bundle.json";

/// bundle.json: what a bundle is
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Info {
    /// A release number that only grows (the release's UTC time, YYYYMMDDHHMM)
    pub version: u64,
    /// linux-x64 or windows-x64
    pub platform: String,
    /// The nxnative repository and revision it was made from
    #[serde(default)]
    pub nxnative: String,
    /// Its API (update::BUNDLE_API: what the app expects of it); 1 for the bundles from before
    #[serde(default = "api_one")]
    pub api: u32,
}
fn api_one() -> u32 {
    1
}

#[derive(Debug, Clone)]
pub struct Payload {
    exe: PathBuf,
    offset: u64,
    len: u64,
    pub hash: String,
    pub version: u64,
}

/// The payload appended to this executable, if any (a development build and an updated client have none)
pub fn locate() -> Option<Payload> {
    let exe = std::env::current_exe().ok()?;
    let mut f = File::open(&exe).ok()?;
    let size = f.metadata().ok()?.len();
    if size < TRAILER2 {
        return None;
    }
    f.seek(SeekFrom::Start(size - TRAILER2)).ok()?;
    let mut t = [0u8; TRAILER2 as usize];
    f.read_exact(&mut t).ok()?;
    if &t[32..40] == MAGIC2 {
        let len = u64::from_le_bytes(t[0..8].try_into().unwrap());
        let version = u64::from_le_bytes(t[8..16].try_into().unwrap());
        let hash = String::from_utf8_lossy(&t[16..32]).into_owned();
        let offset = (size - TRAILER2).checked_sub(len)?;
        return Some(Payload { exe, offset, len, hash, version });
    }
    let t = &t[8..];   // the last 32 bytes: an older app's trailer
    if &t[24..32] != MAGIC {
        return None;
    }
    let len = u64::from_le_bytes(t[0..8].try_into().unwrap());
    let hash = String::from_utf8_lossy(&t[8..24]).into_owned();
    let offset = (size - TRAILER).checked_sub(len)?;
    Some(Payload { exe, offset, len, hash, version: 0 })
}

pub fn dir(store: &Store, p: &Payload) -> PathBuf {
    store.root().join("bundle").join(&p.hash)
}

/// A complete bundle's version (0 for one from before bundle.json)
pub fn info(dir: &Path) -> Info {
    fs::read(dir.join(INFO)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// The complete bundles in the data folder, newest first
fn installed(store: &Store) -> Vec<(u64, PathBuf)> {
    let mut v: Vec<(u64, PathBuf)> = fs::read_dir(store.root().join("bundle"))
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join(COMPLETE).is_file()).map(|p| (info(&p).version, p)).collect())
        .unwrap_or_default();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v
}

/// The active bundle: the newest complete one, unless this app carries a newer one that is not unpacked yet (then None: ensure unpacks it)
pub fn ready(store: &Store) -> Option<PathBuf> {
    let newest = installed(store).into_iter().next();
    match (locate(), newest) {
        (Some(p), Some((v, _))) if p.version > v && !dir(store, &p).join(COMPLETE).is_file() => None,
        (_, Some((_, d))) => Some(d),
        (Some(p), None) => {
            let d = dir(store, &p);
            d.join(COMPLETE).is_file().then_some(d)
        }
        (None, None) => None,
    }
}

/// The active bundle's version, if there is one
pub fn active_version(store: &Store) -> Option<u64> {
    ready(store).map(|d| info(&d).version).or_else(|| locate().map(|p| p.version))
}

/// Unpacks this app's payload unless a bundle at least as new is there already; `progress(compressed bytes read, total)`. None for a development
/// build without any bundle.
pub fn ensure(store: &Store, progress: &(dyn Fn(u64, u64) + Sync)) -> Result<Option<PathBuf>> {
    if let Some(d) = ready(store) {
        return Ok(Some(d));
    }
    let Some(p) = locate() else { return Ok(None) };
    let mut f = File::open(&p.exe)?;
    f.seek(SeekFrom::Start(p.offset))?;
    let d = unpack(store, f.take(p.len), p.len, &p.hash, progress)?;
    Ok(Some(d))
}

/// Unpacks a bundle archive (an update's `bundle-<platform>.tar.zst`) as `<data>/bundle/<hash>` and makes it the active one
pub fn install_archive(store: &Store, archive: &Path, hash: &str, progress: &(dyn Fn(u64, u64) + Sync)) -> Result<PathBuf> {
    ensure!(hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()), "a bundle's folder name is 16 hex digits");
    let len = fs::metadata(archive)?.len();
    unpack(store, File::open(archive)?, len, hash, progress)
}

fn unpack(store: &Store, src: impl Read, len: u64, hash: &str, progress: &(dyn Fn(u64, u64) + Sync)) -> Result<PathBuf> {
    let parent = store.root().join("bundle");
    let d = parent.join(hash);
    fs::create_dir_all(&parent)?;
    let tmp = parent.join(format!("{hash}.partial"));
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    fs::create_dir_all(&tmp)?;
    let counted = Counting { inner: src, read: 0, total: len, progress };
    let mut ar = tar::Archive::new(zstd::stream::read::Decoder::new(counted)?);
    ar.set_preserve_permissions(true);
    ar.unpack(&tmp).context("unpack the bundle")?;
    fs::write(tmp.join(COMPLETE), hash)?;
    if d.exists() {
        fs::remove_dir_all(&d)?;
    }
    fs::rename(&tmp, &d)?;
    // the other bundles: an active one is the newest, so older ones go (and so does this one, if an even newer one is there)
    let keep = installed(store).into_iter().next().map(|(_, p)| p).unwrap_or_else(|| d.clone());
    for e in fs::read_dir(&parent)?.filter_map(|e| e.ok()) {
        if e.path() != keep {
            let _ = fs::remove_dir_all(e.path());
        }
    }
    Ok(keep)
}

struct Counting<'a, R> {
    inner: R,
    read: u64,
    total: u64,
    progress: &'a (dyn Fn(u64, u64) + Sync),
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        (self.progress)(self.read, self.total);
        Ok(n)
    }
}

/// Paths inside an unpacked bundle
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn new(root: &Path) -> Self {
        Self { root: root.to_path_buf() }
    }

    pub fn nxnative(&self) -> PathBuf {
        self.root.join("nxnative")
    }

    pub fn python(&self) -> PathBuf {
        if cfg!(windows) { self.root.join("python/python.exe") } else { self.root.join("python/bin/python3") }
    }

    pub fn dotnet(&self) -> PathBuf {
        self.root.join("dotnet")
    }

    pub fn nx_extract(&self) -> PathBuf {
        self.root.join("prebuilt/nx_extract").join(if cfg!(windows) { "nx_extract.exe" } else { "nx_extract" })
    }

    pub fn swift_object(&self) -> PathBuf {
        self.root.join("prebuilt/swift_DmIOS.o")
    }

    /// Folders that go first in PATH: spirv-cross, clang and lld, the .NET runtime's `dotnet`, Python (build_ipa.sh runs `python3`)
    pub fn bin_dirs(&self) -> Vec<PathBuf> {
        let py = if cfg!(windows) { self.root.join("python") } else { self.root.join("python/bin") };
        vec![self.root.join("bin"), self.root.join("llvm/bin"), self.dotnet(), py]
    }

    /// The environment the pipeline runs in
    pub fn env(&self) -> Vec<(String, String)> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined = std::env::join_paths(self.bin_dirs().into_iter().chain(std::env::split_paths(&path))).unwrap_or(path);
        let s = |p: PathBuf| p.to_string_lossy().into_owned();
        let mut v = vec![
            ("PATH".into(), joined.to_string_lossy().into_owned()),
            ("DOTNET_ROOT".into(), s(self.dotnet())),
            ("DOTNET_CLI_TELEMETRY_OPTOUT".into(), "1".into()),
            ("DOTNET_NOLOGO".into(), "1".into()),
            ("DM_NX_EXTRACT".into(), s(self.nx_extract())),
        ];
        if self.swift_object().is_file() {
            v.push(("DM_SWIFT_OBJECT".into(), s(self.swift_object())));
        }
        // the shared-font stand-ins: the bundle's DejaVu Sans (Noto Sans in bundles before API 2), not whatever fonts the system has;
        // the pipeline may take a system font that covers more languages for STD_FONT (fonts::standard)
        for (var, files) in [("STD_FONT", ["DejaVuSans.ttf", "NotoSans-Regular.ttf"]), ("LETTER_FONT", ["DejaVuSans-Bold.ttf", "NotoSans-Bold.ttf"])] {
            if let Some(f) = files.iter().map(|f| self.root.join("fonts").join(f)).find(|f| f.is_file()) {
                v.push((var.into(), s(f)));
            }
        }
        // native decoders for the pipeline's Python (crates/codecs; shader_split.py decodes Yaz0 in Python without them)
        let codecs = self.root.join("lib").join(if cfg!(windows) { CODECS_WINDOWS } else { CODECS_UNIX });
        if codecs.is_file() {
            v.push(("DM_CODECS".into(), s(codecs)));
        }
        if cfg!(windows) {
            // the translator's launcher (patches/nxnative/windows): an .exe that runs <its name>.dll with dotnet
            v.push(("DM_DOTNET_LAUNCHER".into(), s(self.root.join("bin/dotnet-dll-launcher.exe"))));
        }
        v
    }
}

/// The native decoder library in a bundle's lib/ (crates/codecs)
pub const CODECS_UNIX: &str = "libdmcodecs.so";
pub const CODECS_WINDOWS: &str = "dmcodecs.dll";

/// Checks a bundle folder has what the pipeline needs
pub fn check(p: &Paths) -> Result<()> {
    for (what, path) in [("nxnative", p.nxnative().join("scripts/build_ipa_from_nsp.py")), ("Python", p.python()), ("nx_extract", p.nx_extract())] {
        ensure!(path.exists(), "the app's bundle has no {what} ({})", path.display());
    }
    Ok(())
}
