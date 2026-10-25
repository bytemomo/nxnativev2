//! Copying a game into LiveContainer over the USB cable, much faster than its Wi-Fi download.
//! The device cannot reach this computer through the cable (usbmuxd only connects from the computer to the device), so instead of
//! serving the IPA, this writes the app straight into LiveContainer's `Documents/Applications` through house_arrest + AFC, as Finder's
//! file sharing does (LiveContainer has `UIFileSharingEnabled`, so its Documents are reachable however it was signed).
//! LiveContainer lists every `<bundle ID>.app` folder there when it starts, and patches and signs an app whose `LCPatchRevision` is
//! missing at its first launch, as for the apps it installs itself. A game already there keeps its `LCAppInfo.plist` (the link to its
//! data container, so its saves), with the patch revision dropped so the new executable is patched and signed again.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use idevice::{
    IdeviceService,
    afc::{AfcClient, opcode::AfcFopenMode},
    house_arrest::HouseArrestClient,
    installation_proxy::InstallationProxyClient,
    provider::UsbmuxdProvider,
};
use tokio::sync::mpsc;

use crate::apple::{provider, report};

const APPS: &str = "/Documents/Applications";
/// AFC answers every write before the next one: a few connections at once keep the cable busy
const STREAMS: usize = 4;
const CHUNK: usize = 4 << 20;

#[derive(Debug, Clone)]
pub struct LiveContainer {
    pub bundle_id: String,
    pub name: String,
    pub version: String,
}

/// The LiveContainer installs on the device (a sideloaded one carries the team ID: com.kdt.livecontainer.ABCDE12345), the main one first
pub async fn find(udid: &str) -> Result<Vec<LiveContainer>> {
    let p = provider(udid).await?;
    let mut ip = InstallationProxyClient::connect(&p).await.map_err(report).context("list the device's apps")?;
    let apps = ip.get_apps(Some("User"), None).await.map_err(report)?;
    let mut out = vec![];
    for (id, info) in apps {
        let get = |k: &str| info.as_dictionary().and_then(|d| d.get(k)).and_then(|v| v.as_string()).unwrap_or_default().to_string();
        if get("CFBundleExecutable") == "LiveContainer" || id.to_lowercase().contains("livecontainer") {
            let name = Some(get("CFBundleDisplayName")).filter(|s| !s.is_empty()).unwrap_or_else(|| get("CFBundleName"));
            out.push(LiveContainer { bundle_id: id, name, version: get("CFBundleShortVersionString") });
        }
    }
    out.sort_by_key(|l| (l.bundle_id.len(), l.bundle_id.clone()));
    Ok(out)
}

/// The app folders in a LiveContainer (`<bundle ID>.app`; read only)
pub async fn apps(udid: &str, lc: &str) -> Result<Vec<String>> {
    let mut afc = documents(&provider(udid).await?, lc).await?;
    let mut v: Vec<String> = afc.list_dir(APPS).await.map_err(report)?.into_iter().filter(|n| n.ends_with(".app")).collect();
    v.sort();
    Ok(v)
}

/// The apps in a LiveContainer with the time each was copied or installed (Unix seconds; read only)
pub async fn contents(udid: &str, lc: &str) -> Result<Vec<(String, i64)>> {
    let mut afc = documents(&provider(udid).await?, lc).await?;
    let mut out = vec![];
    for n in afc.list_dir(APPS).await.map_err(report)? {
        if let Some(bundle) = n.strip_suffix(".app") {
            let t = afc.get_file_info(format!("{APPS}/{n}")).await.map(|i| i.modified.and_utc().timestamp()).unwrap_or(0);
            out.push((bundle.to_string(), t));
        }
    }
    out.sort();
    Ok(out)
}

/// LiveContainer+SideStore (what iloader installs) keeps SideStore's folder in its Documents; plain LiveContainer does not
pub async fn has_sidestore(udid: &str, lc: &str) -> bool {
    let Ok(p) = provider(udid).await else { return false };
    let Ok(mut afc) = documents(&p, lc).await else { return false };
    afc.get_file_info("/Documents/SideStore").await.is_ok()
}

/// LiveContainer's own IPA from its GitHub release: `version` (the installed one; the latest release when that tag has none), with
/// SideStore inside or not. Kept in `<data>/apple/livecontainer/<version>/`.
pub async fn download_livecontainer(store: &crate::store::Store, version: &str, sidestore: bool, progress: Arc<dyn Fn(u64, u64) + Send + Sync>) -> Result<PathBuf> {
    crate::logging::tls();
    let asset = if sidestore { "LiveContainer+SideStore.ipa" } else { "LiveContainer.ipa" };
    let dir = store.root().join("apple/livecontainer").join(if version.is_empty() { "latest" } else { version });
    let out = dir.join(asset);
    if out.is_file() {
        return Ok(out);
    }
    std::fs::create_dir_all(&dir)?;
    let base = "https://github.com/LiveContainer/LiveContainer/releases";
    let client = reqwest::Client::builder().user_agent("nxnativev2").build()?;
    let mut urls = vec![];
    if !version.is_empty() {
        urls.push(format!("{base}/download/{version}/{asset}"));
    }
    urls.push(format!("{base}/latest/download/{asset}"));
    let mut last = anyhow!("no download");
    for url in urls {
        tracing::info!(%url, "LiveContainer: downloading its IPA");
        let mut resp = match client.get(&url).send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => r,
            Err(e) => {
                last = anyhow!("{url}: {e}");
                continue;
            }
        };
        let total = resp.content_length().unwrap_or(0);
        let tmp = out.with_extension("partial");
        let mut f = std::fs::File::create(&tmp)?;
        let mut done = 0;
        while let Some(b) = resp.chunk().await? {
            std::io::Write::write_all(&mut f, &b)?;
            done += b.len() as u64;
            progress(done, total);
        }
        drop(f);
        std::fs::rename(&tmp, &out)?;
        return Ok(out);
    }
    Err(last.context("download LiveContainer"))
}

/// More RAM for LiveContainer, and so for every app it runs, in one go: its App ID gets the Increased Memory Limit and LiveContainer is
/// signed and installed again over itself (the same version from its GitHub release; its apps and data stay), since only a new signature
/// carries the entitlement. `say` follows the steps (text, 0..1). Returns what the account's App ID shows afterwards.
pub async fn more_ram(session: &mut crate::apple::Session, store: &crate::store::Store, udid: &str, say: Arc<dyn Fn(String, f32) + Send + Sync>) -> Result<String> {
    reinstall(session, store, udid, true, say).await
}

/// Installs LiveContainer with the account: over the one on the device (the same version and kind, with SideStore or not; its apps and
/// data stay), or, when there is none, the latest release with SideStore (as iloader installs it). `memory`: ask for the Increased Memory
/// Limit. Returns a line on what was done.
pub async fn reinstall(session: &mut crate::apple::Session, store: &crate::store::Store, udid: &str, memory: bool, say: Arc<dyn Fn(String, f32) + Send + Sync>) -> Result<String> {
    say("Looking for LiveContainer on the device…".into(), 0.0);
    let current = find(udid).await?.into_iter().next();
    let team = session.team_id().await?;
    let (version, sidestore) = match &current {
        Some(lc) => {
            if !lc.bundle_id.ends_with(&format!(".{team}")) {
                bail!(
                    "LiveContainer on the device ({}) was not installed with this Apple ID (team {team}): sign in with the Apple ID it was installed with",
                    lc.bundle_id
                );
            }
            (lc.version.clone(), has_sidestore(udid, &lc.bundle_id).await)
        }
        None => (String::new(), true),
    };
    tracing::info!(current = ?current.as_ref().map(|l| &l.bundle_id), version = %version, sidestore, memory, "installing LiveContainer");
    let (s2, v2) = (say.clone(), if version.is_empty() { "latest".to_string() } else { version.clone() });
    let ipa = download_livecontainer(store, &version, sidestore, Arc::new(move |d, t| {
        s2(format!("Downloading LiveContainer {v2}… {:.0} MB", d as f64 / 1e6), if t > 0 { 0.3 * d as f32 / t as f32 } else { 0.0 })
    }))
    .await?;
    let s3 = say.clone();
    crate::apple::install(session, udid, &ipa, memory, Arc::new(move |f| s3(format!("Signing and installing… {:.0}%", f * 100.0), 0.3 + 0.7 * f))).await?;
    let now = find(udid).await.ok().and_then(|l| l.into_iter().next());
    let id = now.as_ref().map(|l| l.bundle_id.clone()).unwrap_or_default();
    let has = session.app_ids().await.ok().and_then(|ids| ids.into_iter().find(|a| a.identifier == id)).and_then(|a| a.memory_limit);
    let v = now.map(|l| l.version).unwrap_or(version);
    Ok(match (memory, has) {
        (true, Some(false)) => format!("LiveContainer {v} installed, but Apple did not add the Increased Memory Limit (see the log)"),
        (true, _) => format!("LiveContainer {v} installed with more RAM"),
        (false, _) => format!("LiveContainer {v} installed"),
    })
}

/// The app inside an IPA: its folder in the archive (`Payload/X.app/`), bundle ID and the entries under it
struct Plan {
    bundle_id: String,
    executable: String,
    dirs: BTreeSet<String>,
    /// (archive index, path under the app folder, size, symlink)
    files: Vec<(usize, String, u64, bool)>,
    /// path → (size, CRC-32) from the archive's directory: the manifest of what is sent
    sums: BTreeMap<String, (u64, u32)>,
    total: u64,
}

fn plan(ipa: &Path) -> Result<Plan> {
    let mut zip = zip::ZipArchive::new(File::open(ipa).with_context(|| format!("open {}", ipa.display()))?).context("read the IPA")?;
    let prefix = (0..zip.len())
        .filter_map(|i| zip.name_for_index(i).map(str::to_string))
        .find_map(|n| {
            let rest = n.strip_prefix("Payload/")?;
            let end = rest.find(".app/")?;
            Some(format!("Payload/{}.app/", &rest[..end]))
        })
        .context("no Payload/<name>.app in the IPA")?;
    let info: plist::Value = {
        let mut f = zip.by_name(&format!("{prefix}Info.plist")).context("no Info.plist in the app")?;
        let mut b = vec![];
        f.read_to_end(&mut b)?;
        plist::from_bytes(&b).context("read the app's Info.plist")?
    };
    let bundle_id = info.as_dictionary().and_then(|d| d.get("CFBundleIdentifier")).and_then(|v| v.as_string()).context("no CFBundleIdentifier")?.to_string();
    let executable = info.as_dictionary().and_then(|d| d.get("CFBundleExecutable")).and_then(|v| v.as_string()).unwrap_or_default().to_string();
    let (mut dirs, mut files, mut total, mut sums) = (BTreeSet::new(), vec![], 0, BTreeMap::new());
    for i in 0..zip.len() {
        let f = zip.by_index_raw(i)?;
        let Some(rel) = f.name().strip_prefix(&prefix).map(|r| r.trim_end_matches('/').to_string()) else { continue };
        if rel.is_empty() || rel.split('/').any(|c| c == ".." || c.is_empty()) {
            continue;
        }
        let parent = |p: &str| p.rsplit_once('/').map(|(d, _)| d.to_string());
        let mut d = if f.is_dir() { Some(rel.clone()) } else { parent(&rel) };
        while let Some(x) = d {
            d = parent(&x);
            dirs.insert(x);
        }
        if !f.is_dir() {
            total += f.size();
            sums.insert(rel.clone(), (f.size(), f.crc32()));
            files.push((i, rel, f.size(), f.is_symlink()));
        }
    }
    Ok(Plan { bundle_id, executable, dirs, files, sums, total })
}

pub(crate) async fn documents(p: &UsbmuxdProvider, lc: &str) -> Result<AfcClient> {
    let ha = HouseArrestClient::connect(p).await.map_err(report).context("open house_arrest")?;
    ha.vend_documents(lc.to_string()).await.map_err(|e| anyhow!("LiveContainer's Documents are not reachable over USB ({e}): is {lc} installed and unlocked?"))
}

enum Msg {
    Open(String),
    Data(Vec<u8>),
    Close,
    Link(String, String),
    Failed(String),
}

/// Reads the given entries of the IPA on a thread of its own, file by file in chunks
fn reader(ipa: PathBuf, files: Vec<(usize, String, u64, bool)>, root: String, tx: mpsc::Sender<Msg>, cancel: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let run = || -> Result<()> {
            let mut zip = zip::ZipArchive::new(File::open(&ipa)?)?;
            for (i, rel, _, link) in files {
                if cancel.load(Ordering::Relaxed) {
                    bail!("cancelled");
                }
                let mut f = zip.by_index(i)?;
                let path = format!("{root}/{rel}");
                if link {
                    let mut t = String::new();
                    f.read_to_string(&mut t)?;
                    tx.blocking_send(Msg::Link(t, path))?;
                    continue;
                }
                tx.blocking_send(Msg::Open(path))?;
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        bail!("cancelled");
                    }
                    let mut buf = vec![0u8; CHUNK];
                    let mut n = 0;
                    while n < CHUNK {
                        match f.read(&mut buf[n..])? {
                            0 => break,
                            k => n += k,
                        }
                    }
                    if n == 0 {
                        break;
                    }
                    buf.truncate(n);
                    tx.blocking_send(Msg::Data(buf))?;
                    if n < CHUNK {
                        break;
                    }
                }
                tx.blocking_send(Msg::Close)?;
            }
            Ok(())
        };
        if let Err(e) = run() {
            let _ = tx.blocking_send(Msg::Failed(format!("{e:#}")));
        }
    });
}

/// Writes what one reader sends through one AFC connection
async fn writer(mut afc: AfcClient, mut rx: mpsc::Receiver<Msg>, done: Arc<AtomicU64>, progress: Arc<dyn Fn(u64) + Send + Sync>) -> Result<()> {
    while let Some(m) = rx.recv().await {
        match m {
            Msg::Open(path) => {
                let mut fd = afc.open(path.clone(), AfcFopenMode::WrOnly).await.map_err(report).with_context(|| format!("create {path}"))?;
                loop {
                    match rx.recv().await {
                        Some(Msg::Data(b)) => {
                            fd.write_entire(&b).await.map_err(report).with_context(|| format!("write {path}"))?;
                            progress(done.fetch_add(b.len() as u64, Ordering::Relaxed) + b.len() as u64);
                        }
                        Some(Msg::Close) => break,
                        Some(Msg::Failed(e)) => bail!("{e}"),
                        _ => bail!("the IPA reader stopped in {path}"),
                    }
                }
                fd.close().await.map_err(report)?;
            }
            Msg::Link(target, path) => afc.link(target, path.clone(), idevice::afc::opcode::LinkType::Symlink).await.map_err(report).with_context(|| format!("link {path}"))?,
            Msg::Failed(e) => bail!("{e}"),
            Msg::Data(_) | Msg::Close => bail!("the IPA reader sent data out of a file"),
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct Copied {
    /// The app's folder in LiveContainer's Documents
    pub folder: String,
    /// It replaced the same game (whose LiveContainer settings and data it keeps)
    pub replaced: bool,
    /// Only what changed was sent (the manifest of the last copy was there)
    pub diff: bool,
    pub sent_bytes: u64,
    pub sent_files: usize,
    pub total_bytes: u64,
}

/// What was sent last, beside the app (outside its bundle, so LiveContainer's signing never sees it): one line per file, size, CRC-32 and path
fn manifest_path(name: &str) -> String {
    format!("{APPS}/.nxnativev2-{name}.manifest")
}

fn manifest_text(sums: &BTreeMap<String, (u64, u32)>) -> String {
    sums.iter().map(|(p, (n, c))| format!("{n}\t{c:08x}\t{p}\n")).collect()
}

fn manifest_parse(t: &str) -> BTreeMap<String, (u64, u32)> {
    t.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '\t');
            Some((it.next()?.parse().ok()?, u32::from_str_radix(it.next()?, 16).ok()?, it.next()?.to_string()))
        })
        .map(|(n, c, p)| (p, (n, c)))
        .collect()
}

async fn write_file(afc: &mut AfcClient, path: &str, data: &[u8]) -> Result<()> {
    let mut fd = afc.open(path.to_string(), AfcFopenMode::WrOnly).await.map_err(report)?;
    fd.write_entire(data).await.map_err(report)?;
    fd.close().await.map_err(report)?;
    Ok(())
}

/// Copies the app in `ipa` into the LiveContainer `lc` (a bundle ID from [`find`]) on device `udid`. When the game is there already with the
/// manifest of its last copy, only the files that changed (size or CRC-32) are sent, into a hidden staging folder, then moved into place;
/// files the new build no longer has are removed. The executable always goes with Info.plist (LiveContainer patches and signs both).
/// `progress(bytes written, bytes to write)`; `cancel` stops it and removes what was staged.
pub async fn copy(udid: &str, lc: &str, ipa: &Path, progress: Arc<dyn Fn(u64, u64) + Send + Sync>, cancel: Arc<AtomicBool>) -> Result<Copied> {
    crate::logging::tls();
    let ipa_path = ipa.to_path_buf();
    let plan = tokio::task::spawn_blocking(move || plan(&ipa_path)).await??;
    let p = provider(udid).await?;
    let mut afc = documents(&p, lc).await?;
    let name: String = plan.bundle_id.chars().filter(|c| c.is_ascii()).collect();
    let folder = format!("{APPS}/{name}.app");
    let tmp = format!("{APPS}/.nxnativev2-{name}.partial");
    let replaced = afc.get_file_info(folder.clone()).await.is_ok();

    // what to send: everything, or what differs from the manifest of the last copy
    let old = if replaced {
        match afc.open(manifest_path(&name), AfcFopenMode::RdOnly).await {
            Ok(mut fd) => {
                let b = fd.read_entire().await.ok();
                let _ = fd.close().await;
                b.map(|b| manifest_parse(&String::from_utf8_lossy(&b)))
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let diff = old.is_some();
    let (send, remove): (Vec<(usize, String, u64, bool)>, Vec<String>) = match &old {
        Some(old) => {
            let exe_changed = old.get(&plan.executable) != plan.sums.get(&plan.executable);
            let send = plan
                .files
                .iter()
                .filter(|f| old.get(&f.1) != plan.sums.get(&f.1) || (exe_changed && f.1 == "Info.plist"))
                .cloned()
                .collect();
            (send, old.keys().filter(|p| !plan.sums.contains_key(*p)).cloned().collect())
        }
        None => (plan.files.clone(), vec![]),
    };
    let exe_sent = send.iter().any(|f| f.1 == plan.executable);
    let total: u64 = send.iter().map(|f| f.2).sum();
    let free = afc.get_device_info().await.map_err(report)?.free_bytes as u64;
    tracing::info!(device = udid, livecontainer = lc, app = %plan.bundle_id, diff, files = send.len(), of = plan.files.len(), bytes = total, removed = remove.len(), free, "LiveContainer USB copy: start");
    if free < total + (512 << 20) {
        bail!("the device has {:.1} GB free; the game needs {:.1} GB", free as f64 / 1e9, total as f64 / 1e9);
    }
    // the staging folder LiveContainer does not list (no .app): the whole app, or the changed files
    let _ = afc.mk_dir(APPS).await;
    let _ = afc.remove_all(tmp.clone()).await;
    afc.mk_dir(tmp.clone()).await.map_err(report).with_context(|| format!("create {tmp}"))?;
    let parent = |p: &str| p.rsplit_once('/').map(|(d, _)| d.to_string());
    let mut dirs = BTreeSet::new();
    if diff {
        for f in &send {
            let mut d = parent(&f.1);
            while let Some(x) = d {
                d = parent(&x);
                dirs.insert(x);
            }
        }
    } else {
        dirs = plan.dirs.clone();
    }

    let result = async {
        for d in &dirs {
            afc.mk_dir(format!("{tmp}/{d}")).await.map_err(report).with_context(|| format!("create {d}"))?;
        }
        // the files shared out over the streams, largest first to the least loaded one
        let mut sorted = send.clone();
        sorted.sort_by_key(|f| std::cmp::Reverse(f.2));
        let mut buckets: Vec<(u64, Vec<_>)> = (0..STREAMS).map(|_| (0, vec![])).collect();
        for f in sorted {
            let b = buckets.iter_mut().min_by_key(|b| b.0).unwrap();
            b.0 += f.2;
            b.1.push(f);
        }
        let done = Arc::new(AtomicU64::new(0));
        let report_progress: Arc<dyn Fn(u64) + Send + Sync> = {
            let progress = progress.clone();
            Arc::new(move |d| progress(d, total))
        };
        let mut writers = vec![];
        for (_, files) in buckets.into_iter().filter(|b| !b.1.is_empty()) {
            let (tx, rx) = mpsc::channel(4);
            reader(ipa.to_path_buf(), files, tmp.clone(), tx, cancel.clone());
            writers.push(writer(documents(&p, lc).await?, rx, done.clone(), report_progress.clone()));
        }
        futures::future::try_join_all(writers).await?;
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }

        if diff {
            // the changed files into the game's folder, the removed ones out; its LiveContainer settings stay where they are
            for d in &dirs {
                let _ = afc.mk_dir(format!("{folder}/{d}")).await;
            }
            for f in &send {
                let to = format!("{folder}/{}", f.1);
                let _ = afc.remove(to.clone()).await;
                afc.rename(format!("{tmp}/{}", f.1), to.clone()).await.map_err(report).with_context(|| format!("move {to} into place"))?;
            }
            for r in &remove {
                let _ = afc.remove(format!("{folder}/{r}")).await;
            }
            let _ = afc.remove_all(tmp.clone()).await;
            if exe_sent {
                if let Some(mut info) = read_plist(&mut afc, &format!("{folder}/LCAppInfo.plist")).await {
                    if let Some(d) = info.as_dictionary_mut() {
                        d.remove("LCPatchRevision");
                    }
                    let mut b = vec![];
                    plist::to_writer_binary(&mut b, &info)?;
                    write_file(&mut afc, &format!("{folder}/LCAppInfo.plist"), &b).await?;
                }
            }
        } else {
            // the same game already there without a manifest: keep its LiveContainer settings (data container, options), patched and signed again at launch
            if replaced {
                if let Some(mut info) = read_plist(&mut afc, &format!("{folder}/LCAppInfo.plist")).await {
                    if let Some(d) = info.as_dictionary_mut() {
                        d.remove("LCPatchRevision");
                    }
                    let mut b = vec![];
                    plist::to_writer_binary(&mut b, &info)?;
                    write_file(&mut afc, &format!("{tmp}/LCAppInfo.plist"), &b).await?;
                    tracing::info!("LiveContainer USB copy: kept the game's LCAppInfo.plist (its data container and settings)");
                }
                afc.remove_all(folder.clone()).await.map_err(report).with_context(|| format!("remove the old {folder}"))?;
            }
            afc.rename(tmp.clone(), folder.clone()).await.map_err(report).with_context(|| format!("rename to {folder}"))?;
        }
        write_file(&mut afc, &manifest_path(&name), manifest_text(&plan.sums).as_bytes()).await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            tracing::info!(folder = %folder, replaced, diff, files = send.len(), bytes = total, "LiveContainer USB copy: done");
            Ok(Copied { folder, replaced, diff, sent_bytes: total, sent_files: send.len(), total_bytes: plan.total })
        }
        Err(e) => {
            tracing::error!("LiveContainer USB copy failed: {e:#}");
            let _ = afc.remove_all(tmp).await;
            Err(e)
        }
    }
}

async fn read_plist(afc: &mut AfcClient, path: &str) -> Option<plist::Value> {
    let mut fd = afc.open(path.to_string(), AfcFopenMode::RdOnly).await.ok()?;
    let b = fd.read_entire().await.ok();
    let _ = fd.close().await;
    plist::from_bytes(&b?).ok()
}

#[cfg(test)]
mod tests {
    /// NXNATIVE_TEST_LC=<udid>/<bundle ID>: what LiveContainer's Documents hold (read only)
    #[tokio::test]
    async fn list_documents() {
        let Ok(v) = std::env::var("NXNATIVE_TEST_LC") else { return };
        let (udid, lc) = v.split_once('/').unwrap();
        let mut afc = super::documents(&super::provider(udid).await.unwrap(), lc).await.unwrap();
        for d in ["/Documents", "/Documents/Applications"] {
            println!("{d}: {:?}", afc.list_dir(d).await);
        }
    }

    /// NXNATIVE_TEST_LC=<udid>/<bundle ID>: SideStore inside or not (read only), and its IPA downloaded into target/lc-download-test
    #[tokio::test]
    async fn livecontainer_ipa() {
        let Ok(v) = std::env::var("NXNATIVE_TEST_LC") else { return };
        let (udid, lc) = v.split_once('/').unwrap();
        let sidestore = super::has_sidestore(udid, lc).await;
        let version = super::find(udid).await.unwrap().into_iter().find(|l| l.bundle_id == lc).unwrap().version;
        let store = crate::store::Store::new(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/lc-download-test")).unwrap();
        let ipa = super::download_livecontainer(&store, &version, sidestore, std::sync::Arc::new(|_, _| {})).await.unwrap();
        let plan = super::plan(&ipa).unwrap();
        println!("sidestore {sidestore}, version {version}: {} ({} bytes), app {}", ipa.display(), std::fs::metadata(&ipa).unwrap().len(), plan.bundle_id);
    }

    /// NXNATIVE_TEST_LS=<udid>/<LiveContainer bundle ID>/<path under its Documents>: a recursive listing with sizes (read only)
    #[tokio::test]
    async fn list_game_data() {
        let Ok(v) = std::env::var("NXNATIVE_TEST_LS") else { return };
        let mut it = v.splitn(3, '/');
        let (udid, lc, path) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
        let mut afc = super::documents(&super::provider(udid).await.unwrap(), lc).await.unwrap();
        let mut todo = vec![format!("/Documents/{path}")];
        while let Some(d) = todo.pop() {
            for n in afc.list_dir(d.clone()).await.unwrap_or_default() {
                if n == "." || n == ".." { continue; }
                let p = format!("{d}/{n}");
                let i = afc.get_file_info(p.clone()).await.unwrap();
                println!("{:>10}  {}  {}", i.size, i.modified, p.trim_start_matches("/Documents/"));
                if i.st_ifmt == "S_IFDIR" { todo.push(p); }
            }
        }
    }

    /// NXNATIVE_TEST_GET=<udid>/<LiveContainer bundle ID>/<path under its Documents>, NXNATIVE_TEST_OUT=<file>: one file copied off the device (read only)
    #[tokio::test]
    async fn get_file() {
        let (Ok(v), Ok(out)) = (std::env::var("NXNATIVE_TEST_GET"), std::env::var("NXNATIVE_TEST_OUT")) else { return };
        let mut it = v.splitn(3, '/');
        let (udid, lc, path) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
        let mut afc = super::documents(&super::provider(udid).await.unwrap(), lc).await.unwrap();
        let mut fd = afc.open(format!("/Documents/{path}"), super::AfcFopenMode::RdOnly).await.unwrap();
        let b = fd.read_entire().await.unwrap();
        std::fs::write(&out, &b).unwrap();
        println!("{} bytes → {out}", b.len());
    }

    #[test]
    fn manifest_round_trip() {
        let mut m = std::collections::BTreeMap::new();
        m.insert("romfs/a b/c.bin".to_string(), (12345u64, 0xdeadbeefu32));
        m.insert("DmIOS".to_string(), (1u64, 0x1u32));
        assert_eq!(super::manifest_parse(&super::manifest_text(&m)), m);
    }

    /// NXNATIVE_TEST_IPA=<an IPA> cargo test -p nxcore lc_usb -- --nocapture
    #[test]
    fn plan_reads_ipa() {
        let Ok(ipa) = std::env::var("NXNATIVE_TEST_IPA") else { return };
        let p = super::plan(std::path::Path::new(&ipa)).unwrap();
        let links = p.files.iter().filter(|f| f.3).count();
        println!("{}: {} dirs, {} files ({links} links), {:.2} GB", p.bundle_id, p.dirs.len(), p.files.len(), p.total as f64 / 1e9);
        assert!(!p.files.is_empty() && p.files.iter().any(|f| f.1 == "Info.plist"));
    }
}
