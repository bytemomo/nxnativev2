//! Xcode.xip → iPhoneOS SDK, without macOS.
//!
//! An Xcode .xip is a xar archive holding `Content` (a pbzx stream: 16 MiB chunks, each xz or stored) whose
//! concatenation is an odc cpio archive of `./Xcode.app/...`. Only the iPhoneOS SDK subtree is written.
//! Chunks are decoded in parallel, in batches, and fed to the cpio reader in order. Two passes over the stream: Apple's cpio stores a
//! hard link's content only in the first entry of its inode (later entries hold a same-sized placeholder), and that first
//! entry may lie outside the SDK.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

/// Where the SDKs live inside the archive
const SDK_PREFIX: &str = "Xcode.app/Contents/Developer/Platforms/iPhoneOS.platform/Developer/SDKs/";
/// Xcode's Swift resource folder: the SDK's Swift interfaces only build with it (its shims, iphoneos overlays, apinotes)
const TOOL_SWIFT: &str = "Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/";
const TOOL_CLANG: &str = "Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib/clang/";
/// Where the Swift resources go, beside the SDKs (79 MB of the toolchain's 1.3 GB: iphoneos, apinotes, the top-level files, clang/include)
pub const SWIFT_RESOURCES: &str = "swift-resources";

/// Progress of an extraction: bytes of `Content` read and its total size
pub type Progress<'a> = &'a (dyn Fn(u64, u64) + Sync);
type BoxedProgress<'a> = Box<dyn Fn(u64, u64) + 'a>;

/// Extracts the iPhoneOS SDK(s) of `xip` into `dest` (created; existing content is replaced) and returns the path of the
/// versioned SDK (e.g. `dest/iPhoneOS26.5.sdk`)
pub fn extract_ios_sdk(xip: &Path, dest: &Path, progress: Progress) -> Result<PathBuf> {
    let mut f = File::open(xip).with_context(|| format!("open {}", xip.display()))?;
    let (offset, length) = xar_entry(&mut f, "Content").context("not an Xcode .xip (xar archive with a Content entry)")?;
    drop(f);
    let content = |part: u64| -> Result<PbzxReader<'_, _>> {
        let mut f = File::open(xip)?;
        f.seek(SeekFrom::Start(offset))?;
        let half = move |done: u64, total: u64| progress(part * total + done, 2 * total);
        PbzxReader::new(BufReader::with_capacity(1 << 20, f.take(length)), length, Box::new(half))
    };

    let tmp = dest.with_extension("partial");
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    fs::create_dir_all(&tmp)?;
    // pass 1: the SDK's entries; pass 2: their content (a hard link's content is in the first entry of its inode, which may lie outside the SDK)
    let plan = scan(content(0)?)?;
    write(content(1)?, &plan, &tmp)?;

    let sdk = fs::read_dir(&tmp)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("iPhoneOS") && n.ends_with(".sdk") && n != "iPhoneOS.sdk")
        .max()
        .context("the archive holds no iPhoneOS SDK")?;
    ensure!(tmp.join(&sdk).join("SDKSettings.json").is_file(), "the extracted {sdk} has no SDKSettings.json");
    if dest.exists() {
        fs::remove_dir_all(dest)?;
    }
    fs::rename(&tmp, dest)?;
    Ok(dest.join(sdk))
}

/// (offset, length) of a top-level xar entry
fn xar_entry(f: &mut File, name: &str) -> Result<(u64, u64)> {
    let mut h = [0u8; 28];
    f.read_exact(&mut h)?;
    ensure!(&h[0..4] == b"xar!", "no xar magic");
    let header_size = u16::from_be_bytes([h[4], h[5]]) as u64;
    let toc_len = u64::from_be_bytes(h[8..16].try_into().unwrap());
    f.seek(SeekFrom::Start(header_size))?;
    let mut toc = String::new();
    flate2::read::ZlibDecoder::new(Read::by_ref(f).take(toc_len)).read_to_string(&mut toc)?;
    let heap = header_size + toc_len;

    // top-level <file> elements: <name>…</name><data><offset>…</offset><length>…</length>…</data>, nested <file>s skipped
    let (mut depth, mut i) = (0usize, 0usize);
    let mut top_start = 0usize;
    while i < toc.len() {
        let rest = &toc[i..];
        if rest.starts_with("<file") && rest[5..].starts_with(|c: char| c == ' ' || c == '>') {
            if depth == 0 {
                top_start = i;
            }
            depth += 1;
        } else if rest.starts_with("</file>") {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                let block = &toc[top_start..i];
                let inner = |b: &str, tag: &str| -> Option<String> {
                    let a = b.find(&format!("<{tag}>"))? + tag.len() + 2;
                    Some(b[a..a + b[a..].find(&format!("</{tag}>"))?].trim().to_string())
                };
                // the entry's own name comes before any nested <file>
                let head = &block[..block[5..].find("<file").map_or(block.len(), |p| p + 5)];
                if inner(head, "name").as_deref() == Some(name) {
                    let data = inner(head, "data").context("entry has no data")?;
                    let off: u64 = inner(&data, "offset").context("no data offset")?.parse()?;
                    let len: u64 = inner(&data, "length").context("no data length")?.parse()?;
                    return Ok((heap + off, len));
                }
            }
        }
        i += rest.chars().next().map_or(1, char::len_utf8);
    }
    bail!("no {name} entry")
}

/// The decompressed bytes of a pbzx stream. Reads `threads` chunks ahead and decodes them in parallel.
struct PbzxReader<'a, R: Read> {
    src: R,
    total: u64,
    read: u64,
    progress: BoxedProgress<'a>,
    ready: std::collections::VecDeque<Vec<u8>>,
    cur: Vec<u8>,
    pos: usize,
    done: bool,
}

impl<'a, R: Read> PbzxReader<'a, R> {
    fn new(mut src: R, total: u64, progress: BoxedProgress<'a>) -> Result<Self> {
        let mut h = [0u8; 12];
        src.read_exact(&mut h)?;
        ensure!(&h[0..4] == b"pbzx", "Content is not a pbzx stream");
        Ok(Self { src, total, read: 12, progress, ready: Default::default(), cur: Vec::new(), pos: 0, done: false })
    }

    /// Reads up to `n` raw chunks: (uncompressed size, payload)
    fn read_chunks(&mut self, n: usize) -> io::Result<Vec<(u64, Vec<u8>)>> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            let mut h = [0u8; 16];
            match self.src.read_exact(&mut h) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    self.done = true;
                    break;
                }
                Err(e) => return Err(e),
            }
            let usize_ = u64::from_be_bytes(h[0..8].try_into().unwrap());
            let csize = u64::from_be_bytes(h[8..16].try_into().unwrap());
            let mut payload = vec![0u8; csize as usize];
            self.src.read_exact(&mut payload)?;
            self.read += 16 + csize;
            out.push((usize_, payload));
        }
        (self.progress)(self.read, self.total);
        Ok(out)
    }

    fn refill(&mut self) -> io::Result<()> {
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let raw = self.read_chunks(threads * 2)?;
        let decoded: Vec<io::Result<Vec<u8>>> = std::thread::scope(|s| {
            let hs: Vec<_> = raw.into_iter().map(|(u, p)| s.spawn(move || decode_chunk(u, p))).collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for d in decoded {
            self.ready.push_back(d?);
        }
        Ok(())
    }
}

fn decode_chunk(usize_: u64, payload: Vec<u8>) -> io::Result<Vec<u8>> {
    if payload.starts_with(b"\xfd7zXZ\x00") {
        let mut out = Vec::with_capacity(usize_ as usize);
        liblzma::read::XzDecoder::new(&payload[..]).read_to_end(&mut out)?;
        Ok(out)
    } else {
        Ok(payload) // stored: xz did not make it smaller
    }
}

impl<R: Read> Read for PbzxReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.cur.len() {
            if let Some(next) = self.ready.pop_front() {
                self.cur = next;
                self.pos = 0;
            } else if self.done {
                return Ok(0);
            } else {
                self.refill()?;
            }
        }
        let n = buf.len().min(self.cur.len() - self.pos);
        buf[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// One odc cpio header
struct Entry {
    inode: (u64, u64),
    mode: u64,
    size: u64,
    name: String,
}

/// Reads the next header (None at the trailer); the caller consumes or skips its `size` bytes of data
fn next_entry(r: &mut impl Read) -> Result<Option<Entry>> {
    let mut h = [0u8; 76];
    r.read_exact(&mut h).context("truncated cpio header")?;
    ensure!(&h[0..6] == b"070707", "bad cpio magic");
    let oct = |a: usize, b: usize| -> Result<u64> { Ok(u64::from_str_radix(std::str::from_utf8(&h[a..b])?, 8)?) };
    let (dev, ino, mode) = (oct(6, 12)?, oct(12, 18)?, oct(18, 24)?);
    let (namesize, size) = (oct(59, 65)? as usize, oct(65, 76)?);
    let mut name = vec![0u8; namesize];
    r.read_exact(&mut name)?;
    let name = String::from_utf8_lossy(&name[..namesize.saturating_sub(1)]).into_owned();
    Ok((name != "TRAILER!!!").then_some(Entry { inode: (dev, ino), mode, size, name }))
}

/// The output path of an archive entry, None for what is not kept
fn out_rel(name: &str, is_dir: bool) -> Option<String> {
    let n = name.trim_start_matches("./");
    if let Some(r) = n.strip_prefix(SDK_PREFIX).filter(|r| !r.is_empty()) {
        return Some(r.to_string());
    }
    if let Some(r) = n.strip_prefix(TOOL_SWIFT).filter(|r| !r.is_empty()) {
        let top = r.split('/').next().unwrap_or_default();
        let keep = top == "iphoneos" || top == "apinotes" || (!r.contains('/') && !is_dir && top != "clang");
        return keep.then(|| format!("{SWIFT_RESOURCES}/{r}"));
    }
    if let Some(r) = n.strip_prefix(TOOL_CLANG) {
        // <version>/include/…
        let rest = r.split_once('/')?.1;
        // plus clang's runtime for iOS (the __isPlatformVersionAtLeast of #available / @available), which Xcode links into every app
        if rest == "include" || rest.starts_with("include/") || rest == "lib" || rest == "lib/darwin" || rest == "lib/darwin/libclang_rt.ios.a" {
            return Some(format!("{SWIFT_RESOURCES}/clang/{rest}"));
        }
    }
    None
}

fn skip(r: &mut impl Read, n: u64) -> io::Result<()> {
    io::copy(&mut r.take(n), &mut io::sink()).map(|_| ())
}

const S_IFMT: u64 = 0o170000;
const S_IFREG: u64 = 0o100000;

/// What pass 2 writes: for every inode an SDK regular file uses, the SDK paths that share it (the first one gets the content)
#[derive(Default)]
struct Plan {
    files: HashMap<(u64, u64), Vec<String>>,
}

fn scan(mut r: impl Read) -> Result<Plan> {
    let mut plan = Plan::default();
    while let Some(e) = next_entry(&mut r)? {
        skip(&mut r, e.size)?;
        if e.mode & S_IFMT == S_IFREG {
            if let Some(rel) = out_rel(&e.name, false) {
                plan.files.entry(e.inode).or_default().push(rel);
            }
        }
    }
    Ok(plan)
}

fn write(mut r: impl Read, plan: &Plan, out: &Path) -> Result<()> {
    let mut written = std::collections::HashSet::new();
    let mut symlinks = Vec::new();
    let mut buf = vec![0u8; 1 << 20];
    while let Some(e) = next_entry(&mut r)? {
        let kind = e.mode & S_IFMT;
        if kind == S_IFREG && !written.contains(&e.inode) {
            if let Some(paths) = plan.files.get(&e.inode) {
                // the inode's first entry: its data is the content of every SDK path that shares it
                let first = out.join(&paths[0]);
                fs::create_dir_all(first.parent().unwrap())?;
                let mut w = io::BufWriter::new(File::create(&first)?);
                let mut left = e.size;
                while left > 0 {
                    let n = (left as usize).min(buf.len());
                    r.read_exact(&mut buf[..n])?;
                    w.write_all(&buf[..n])?;
                    left -= n as u64;
                }
                w.flush()?;
                for other in &paths[1..] {
                    let p = out.join(other);
                    fs::create_dir_all(p.parent().unwrap())?;
                    fs::hard_link(&first, &p).or_else(|_| fs::copy(&first, &p).map(|_| ()))?;
                }
                written.insert(e.inode);
                continue;
            }
        }
        match (kind, out_rel(&e.name, kind == 0o040000)) {
            (0o040000, Some(rel)) => {
                fs::create_dir_all(out.join(rel))?;
                skip(&mut r, e.size)?;
            }
            (0o120000, Some(rel)) => {
                let mut target = vec![0u8; e.size as usize];
                r.read_exact(&mut target)?;
                symlinks.push((out.join(rel), String::from_utf8_lossy(&target).into_owned()));
            }
            _ => skip(&mut r, e.size)?,
        }
    }
    let missing = plan.files.keys().filter(|k| !written.contains(*k)).count();
    ensure!(missing == 0, "{missing} SDK files had no content in the archive");
    make_symlinks(&symlinks)
}

/// Creates the symlinks: real links on Unix; copies of the target on Windows, where links need privileges
fn make_symlinks(symlinks: &[(PathBuf, String)]) -> Result<()> {
    for (path, target) in symlinks {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if path.symlink_metadata().is_ok() {
            continue;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, path)?;
        #[cfg(not(unix))]
        {
            let src = path.parent().unwrap().join(target);
            if src.is_dir() {
                copy_dir(&src, path)?;
            } else if src.is_file() {
                fs::copy(&src, path)?;
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn copy_dir(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() { copy_dir(&e.path(), &to)? } else { fs::copy(e.path(), to).map(|_| ())? }
    }
    Ok(())
}
