//! What the packaged app carries: a zstd-compressed tar appended to its own executable (`cargo xtask bundle` writes it), unpacked once into
//! `<data>/bundle/<hash>/`. A new build of the app carries another hash and unpacks again (the old folder is removed).
//!
//! ```text
//! nxnative/                 the pinned nxnative revision with the platform's patches applied
//! python/                   portable CPython with the pipeline's modules
//! dotnet/                   the .NET runtime (the shader translator and nx_extract run on it)
//! prebuilt/nx_extract/      nx_extract, compiled ahead of time
//! prebuilt/swift_DmIOS.o    the iOS app's Swift sources, compiled ahead of time (an arm64 iOS object: the same on every computer)
//! bin/                      spirv-cross
//! llvm/bin, llvm/lib/clang  clang and lld (ld64.lld) with clang's headers
//! ```
//!
//! Layout of the executable: `<program><payload: tar.zst><trailer: payload length u64 LE, hash 16 ASCII, magic 8>`.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::store::Store;

pub const MAGIC: &[u8; 8] = b"NXNV2PL1";
pub const TRAILER: u64 = 32;
const COMPLETE: &str = ".nxnativev2-complete";

#[derive(Debug, Clone)]
pub struct Payload {
    exe: PathBuf,
    offset: u64,
    len: u64,
    pub hash: String,
}

/// The payload appended to this executable, if any (a development build has none)
pub fn locate() -> Option<Payload> {
    let exe = std::env::current_exe().ok()?;
    let mut f = File::open(&exe).ok()?;
    let size = f.metadata().ok()?.len();
    if size < TRAILER {
        return None;
    }
    f.seek(SeekFrom::Start(size - TRAILER)).ok()?;
    let mut t = [0u8; TRAILER as usize];
    f.read_exact(&mut t).ok()?;
    if &t[24..32] != MAGIC {
        return None;
    }
    let len = u64::from_le_bytes(t[0..8].try_into().unwrap());
    let hash = String::from_utf8_lossy(&t[8..24]).into_owned();
    let offset = (size - TRAILER).checked_sub(len)?;
    Some(Payload { exe, offset, len, hash })
}

pub fn dir(store: &Store, p: &Payload) -> PathBuf {
    store.root().join("bundle").join(&p.hash)
}

/// The unpacked bundle, if it is complete
pub fn ready(store: &Store) -> Option<PathBuf> {
    let p = locate()?;
    let d = dir(store, &p);
    d.join(COMPLETE).is_file().then_some(d)
}

/// Unpacks the payload unless it is there already; `progress(compressed bytes read, total)`. None for a development build without one.
pub fn ensure(store: &Store, progress: &(dyn Fn(u64, u64) + Sync)) -> Result<Option<PathBuf>> {
    let Some(p) = locate() else { return Ok(None) };
    let d = dir(store, &p);
    if d.join(COMPLETE).is_file() {
        return Ok(Some(d));
    }
    let parent = d.parent().unwrap().to_path_buf();
    fs::create_dir_all(&parent)?;
    let tmp = parent.join(format!("{}.partial", p.hash));
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    fs::create_dir_all(&tmp)?;

    let mut f = File::open(&p.exe)?;
    f.seek(SeekFrom::Start(p.offset))?;
    let counted = Counting { inner: f.take(p.len), read: 0, total: p.len, progress };
    let mut ar = tar::Archive::new(zstd::stream::read::Decoder::new(counted)?);
    ar.set_preserve_permissions(true);
    ar.unpack(&tmp).context("unpack the app's bundle")?;
    fs::write(tmp.join(COMPLETE), &p.hash)?;
    if d.exists() {
        fs::remove_dir_all(&d)?;
    }
    fs::rename(&tmp, &d)?;
    // earlier versions' bundles
    for e in fs::read_dir(&parent)?.filter_map(|e| e.ok()) {
        if e.path() != d {
            let _ = fs::remove_dir_all(e.path());
        }
    }
    Ok(Some(d))
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
        v
    }
}

/// Checks a bundle folder has what the pipeline needs
pub fn check(p: &Paths) -> Result<()> {
    for (what, path) in [("nxnative", p.nxnative().join("scripts/build_ipa_from_nsp.py")), ("Python", p.python()), ("nx_extract", p.nx_extract())] {
        ensure!(path.exists(), "the app's bundle has no {what} ({})", path.display());
    }
    Ok(())
}
