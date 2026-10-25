//! Updates from the GitHub releases of nxnativev2: the client (this executable) and the bundle (bundle.rs), each on its own.
//!
//! A release carries `manifest.json` and its signature `manifest.json.sig` (ed25519, hex), made by `cargo xtask release` with the release key
//! (on the computer that publishes, or in CI from a repository secret). The client trusts only a manifest that PUBLIC_KEY signed; the manifest
//! gives every asset's size and SHA-256, and each bundle's SBOM, so a download that does not match is dropped before anything is installed.
//! Releases are incremental: a release builds only the components whose inputs changed, and its manifest is the
//! latest of everything, each asset with the tag of the release that holds it (per_asset_tags; an asset without one is in the manifest's own
//! release). A bundle states its API (BUNDLE_API: the contract between the app and a bundle); an app installs only bundles whose API it runs.
//! Versions only go up, and a release makes them: the bundle's is the release number (the release's UTC time, YYYYMMDDHHMM), the client's
//! <major>.<release number> (the major is Cargo.toml's, raised by hand; xtask stamps the number into the build, NXNATIVEV2_RELEASE). A bundle
//! may need a newer client (`min_client`): the client is offered first then. A development build (no number) never replaces itself.
//!
//! Assets of a release (xtask release):
//! ```text
//! manifest.json, manifest.json.sig
//! nxnativev2-<platform>[.exe]                 the app with its bundle appended (a first install)
//! <variant>-<platform>-client[.exe]           the app (nxnativev2) or the command line (nxnative-cli) alone: what an update downloads
//! bundle-<platform>.tar.zst                   the bundle, unpacked into <data>/bundle/<hash>
//! bundle-<platform>.cdx.json                  its SBOM (CycloneDX), also attested in CI (GitHub's attestations, Sigstore)
//! nxnativev2-client.cdx.json                  the client's SBOM (its Rust crates)
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::bundle;
use crate::store::Store;

/// The repository whose releases are read
pub const REPO: &str = "bytemomo/nxnativev2";
/// The release key's public half (ed25519, hex): `cargo xtask keygen` printed it
#[cfg(not(feature = "test-update-key"))]
pub const PUBLIC_KEY: &str = "809bcdb3cf32d01fa54e1f28c58bb9edd90cce4e5ad65b2daf00645664d324b5";
/// A test build's key (the feature test-update-key, never in a release): updates signed by a throwaway key, served locally (NXNATIVEV2_UPDATE_URL)
#[cfg(feature = "test-update-key")]
pub const PUBLIC_KEY: &str = env!("NXNATIVEV2_TEST_PUBLIC_KEY");

/// The bundle API this client runs: how the app runs a bundle (the build script's arguments and options, the environment it sets, the bundle's
/// layout and bundle.json). Raise BUNDLE_API with a change to that contract (and BUNDLE_API_MIN when the app no longer runs older bundles)
pub const BUNDLE_API: u32 = 2;   // 2: fonts/ holds DejaVu Sans instead of Noto Sans
pub const BUNDLE_API_MIN: u32 = 1;
fn api_one() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub file: String,
    pub sha256: String,
    pub size: u64,
    /// The release that holds it (absent: the manifest's own)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// The client version it is (absent: the manifest's client version)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleAsset {
    pub file: String,
    pub sha256: String,
    pub size: u64,
    /// The folder it unpacks to (the first 8 bytes of its SHA-256, hex), as an app's own payload
    pub hash: String,
    pub sbom: String,
    pub sbom_sha256: String,
    /// The release that holds it (absent: the manifest's own)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// This platform's bundle version (absent: the manifest's bundle version)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    /// The bundle API it has (absent: 1, the bundles from before)
    #[serde(default = "api_one")]
    pub api: u32,
    /// The hash of what it was built from (xtask release: unchanged inputs, no new bundle)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientRelease {
    pub version: String,
    /// "<variant>-<platform>": nxnativev2-linux-x64, nxnative-cli-windows-x64, ...
    pub assets: BTreeMap<String, Asset>,
    /// The bundle APIs it runs, lowest and highest (absent: 1 only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<[u32; 2]>,
    /// The hash of what it was built from
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleRelease {
    pub version: u64,
    /// The oldest client that runs this bundle
    pub min_client: String,
    /// by platform: linux-x64, windows-x64
    pub assets: BTreeMap<String, BundleAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    /// The release's tag: assets download from releases/download/<tag>/<file>
    pub tag: String,
    pub published: String,
    pub commit: String,
    pub client: ClientRelease,
    pub bundle: BundleRelease,
    /// Assets carry the tag of the release that holds them (incremental releases)
    #[serde(default)]
    pub per_asset_tags: bool,
    /// The packaged apps for a first install, by platform (the app with its bundle; not what updates download)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub installers: BTreeMap<String, Asset>,
    /// What changed, by release, newest first (this one and the ones before it, so an update across several shows them all); its GitHub
    /// release lists the same
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<Changes>,
}

/// One release's changes: the subjects of the commits it was the first to build (without their [ci])
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Changes {
    pub tag: String,
    pub published: String,
    pub notes: Vec<String>,
}

/// How many releases' changes a manifest carries
pub const CHANGES_KEPT: usize = 30;

/// A tag's release number: v0.202610242339 → 202610242339 (0 for another form)
pub fn tag_number(tag: &str) -> u64 {
    tag.rsplit('.').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// The newest release this computer has: the older of this app's and the active bundle's (a development build counts only its bundle)
pub fn installed_release(store: &Store) -> u64 {
    [release_number(), bundle::active_version(store).unwrap_or(0)].into_iter().filter(|&n| n > 0).min().unwrap_or(0)
}

/// The changes of the releases after `since`, newest first
pub fn changes_since(m: &Manifest, since: u64) -> Vec<Changes> {
    m.changes.iter().filter(|c| tag_number(&c.tag) > since).cloned().collect()
}

/// What's new after an update: kept in the data folder until it has been shown (an app update shows it after the restart)
fn whats_new_file(store: &Store) -> PathBuf {
    store.root().join("whats-new.json")
}
pub fn save_whats_new(store: &Store, changes: &[Changes]) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    std::fs::write(whats_new_file(store), serde_json::to_vec_pretty(changes)?)?;
    Ok(())
}
pub fn whats_new(store: &Store) -> Vec<Changes> {
    std::fs::read(whats_new_file(store)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}
pub fn clear_whats_new(store: &Store) {
    let _ = std::fs::remove_file(whats_new_file(store));
}

/// The app release whose changes were last shown (0: none yet). An app updated by one from before what's new existed has no
/// whats-new.json: its first check shows its own release's changes from the manifest instead
fn seen_file(store: &Store) -> PathBuf {
    store.root().join("whats-new-seen")
}
/// This app's release counts as shown (its changes came from whats-new.json)
pub fn mark_seen(store: &Store) {
    if release_number() > 0 {
        let _ = std::fs::write(seen_file(store), release_number().to_string());
    }
}
/// The changes this app's release has not shown yet (`m`: the latest manifest), and marks them shown; none for a development build
pub fn unseen_changes(store: &Store, m: &Manifest) -> Vec<Changes> {
    let now = release_number();
    let seen: u64 = std::fs::read_to_string(seen_file(store)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    if now == 0 || seen >= now {
        return vec![];
    }
    mark_seen(store);
    // the releases after the one last shown, up to this app's; nothing shown yet: only this release's own (not all the manifest keeps)
    m.changes.iter().filter(|c| { let n = tag_number(&c.tag); if seen > 0 { n > seen && n <= now } else { n == now } }).cloned().collect()
}

impl Manifest {
    /// The release an asset is downloaded from
    pub fn tag_of(&self, asset_tag: &Option<String>) -> String {
        if self.per_asset_tags { asset_tag.clone().unwrap_or_else(|| self.tag.clone()) } else { self.tag.clone() }
    }
    /// This platform's bundle version
    pub fn bundle_version(&self, platform: &str) -> Option<u64> {
        self.bundle.assets.get(platform).map(|a| a.version.unwrap_or(self.bundle.version))
    }
}

/// How an install is going: the step ("download" or "unpack") and its bytes done of total
pub type Progress = Arc<dyn Fn(&'static str, u64, u64) + Send + Sync>;

/// What a release has for this computer
#[derive(Debug, Clone)]
pub struct Status {
    pub manifest: Manifest,
    /// A newer client than this one
    pub client: Option<String>,
    /// A newer bundle than the active one (None when this one is as new, or the release has none for this platform)
    pub bundle: Option<u64>,
    /// The newer bundle needs a newer client first
    pub bundle_needs_client: bool,
}

pub fn platform() -> &'static str {
    if cfg!(windows) { "windows-x64" } else { "linux-x64" }
}

/// The release this client was built for (xtask stamps it: NXNATIVEV2_RELEASE); 0 for a development build
pub fn release_number() -> u64 {
    option_env!("NXNATIVEV2_RELEASE").and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// This client's version: <major>.<release number>, or <major>.0-dev for a development build
pub fn client_version() -> String {
    match release_number() {
        0 => format!("{}.0-dev", env!("CARGO_PKG_VERSION_MAJOR")),
        n => format!("{}.{n}", env!("CARGO_PKG_VERSION_MAJOR")),
    }
}

/// a.b.c, compared as numbers (anything after the third number is ignored)
fn semver(v: &str) -> (u64, u64, u64) {
    let mut it = v.trim_start_matches('v').split(['.', '-', '+']).map(|p| p.parse::<u64>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn unhex(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    ensure!(s.len().is_multiple_of(2), "hex of odd length");
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| anyhow!("hex: {e}"))).collect()
}

/// Checks a manifest's signature against the release key
pub fn verify(manifest: &[u8], sig_hex: &str) -> Result<()> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let key: [u8; 32] = unhex(PUBLIC_KEY).context("the built-in release key")?.try_into().map_err(|_| anyhow!("the built-in release key is not 32 bytes"))?;
    let key = VerifyingKey::from_bytes(&key).context("the built-in release key")?;
    let sig: [u8; 64] = unhex(sig_hex)?.try_into().map_err(|_| anyhow!("the manifest's signature is not 64 bytes"))?;
    key.verify(manifest, &Signature::from_bytes(&sig)).context("the release's manifest is not signed by the release key: not installed")
}

/// Where releases are read: GitHub's, or NXNATIVEV2_UPDATE_URL (a test server with the same layout: <url>/latest/download/<file>,
/// <url>/download/<tag>/<file>); what it serves is checked against the release key all the same
pub fn releases() -> String {
    std::env::var("NXNATIVEV2_UPDATE_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| format!("https://github.com/{REPO}/releases"))
}

fn http() -> Result<reqwest::Client> {
    crate::logging::tls();
    Ok(reqwest::Client::builder().user_agent(concat!("nxnativev2/", env!("CARGO_PKG_VERSION"))).build()?)
}

async fn get(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let r = client.get(url).send().await.and_then(|r| r.error_for_status()).with_context(|| format!("download {url}"))?;
    Ok(r.bytes().await?.to_vec())
}

/// The latest release's manifest, verified, and what in it is newer than what this computer has
pub async fn check(store: &Store) -> Result<Status> {
    let client = http()?;
    let base = format!("{}/latest/download", releases());
    let raw = get(&client, &format!("{base}/manifest.json")).await?;
    let sig = String::from_utf8(get(&client, &format!("{base}/manifest.json.sig")).await?)?;
    verify(&raw, &sig)?;
    let manifest: Manifest = serde_json::from_slice(&raw).context("the release's manifest")?;
    ensure!(manifest.schema == 1, "the release's manifest is a newer format (schema {}): update the app by hand", manifest.schema);
    // a development build is not replaced: it is built, not installed
    let newer_client = release_number() > 0 && semver(&manifest.client.version) > semver(&client_version());
    let active = bundle::active_version(store).unwrap_or(0);
    let asset = manifest.bundle.assets.get(platform());
    let api = asset.map_or(1, |a| a.api);
    let bundle = manifest.bundle_version(platform()).filter(|v| *v > active).filter(|_| api >= BUNDLE_API_MIN);
    // a newer bundle this app cannot run (its API), or one only newer apps may take (min_client): the app first
    let bundle_needs_client = bundle.is_some() && (api > BUNDLE_API || semver(&manifest.bundle.min_client) > semver(&client_version()));
    // and a new app is taken only when it runs the release's bundle for this platform
    let client_api = manifest.client.api.unwrap_or([1, 1]);
    let newer_client = newer_client && (asset.is_none() || (client_api[0]..=client_api[1]).contains(&api));
    tracing::info!(tag = %manifest.tag, client = %manifest.client.version, bundle = ?manifest.bundle_version(platform()), api, active, newer_client, "update: checked the latest release");
    Ok(Status { client: newer_client.then(|| manifest.client.version.clone()), bundle, bundle_needs_client, manifest })
}

/// Downloads one asset of the release into `dir`, checking its size and SHA-256 as it goes
async fn fetch(manifest: &Manifest, tag: &Option<String>, file: &str, size: u64, sha256: &str, dir: &Path, progress: &Progress) -> Result<PathBuf> {
    use sha2::Digest;
    ensure!(!file.contains(['/', '\\']) && !file.starts_with('.'), "an asset name with a path: {file}");
    std::fs::create_dir_all(dir)?;
    let out = dir.join(file);
    let tmp = dir.join(format!("{file}.partial"));
    let url = format!("{}/download/{}/{file}", releases(), manifest.tag_of(tag));
    tracing::info!(%url, "update: downloading");
    let mut resp = http()?.get(&url).send().await.and_then(|r| r.error_for_status()).with_context(|| format!("download {url}"))?;
    let mut f = std::fs::File::create(&tmp)?;
    let (mut done, mut h) = (0u64, sha2::Sha256::new());
    while let Some(b) = resp.chunk().await? {
        done += b.len() as u64;
        ensure!(done <= size, "{file} is larger than the manifest says");
        h.update(&b);
        std::io::Write::write_all(&mut f, &b)?;
        progress("download", done, size);
    }
    f.sync_all()?;
    drop(f);
    let got = hex(&h.finalize());
    if done != size || got != sha256 {
        let _ = std::fs::remove_file(&tmp);
        bail!("{file} does not match the signed manifest ({done} bytes, SHA-256 {got}): not installed");
    }
    std::fs::rename(&tmp, &out)?;
    Ok(out)
}

/// Downloads and installs the release's bundle for this platform; the new bundle is the active one from the next build on
pub async fn install_bundle(store: &Store, status: &Status, progress: Progress) -> Result<PathBuf> {
    ensure!(!status.bundle_needs_client, "this bundle needs nxnativev2 {} or newer: update the app first", status.manifest.bundle.min_client);
    let m = &status.manifest;
    let a = m.bundle.assets.get(platform()).with_context(|| format!("the release has no bundle for {}", platform()))?;
    let dir = store.tmp_dir().join("update");
    let archive = fetch(m, &a.tag, &a.file, a.size, &a.sha256, &dir, &progress).await?;
    let (store2, hash, archive2, p) = (store.clone(), a.hash.clone(), archive.clone(), progress.clone());
    let d = tokio::task::spawn_blocking(move || bundle::install_archive(&store2, &archive2, &hash, &|d, t| p("unpack", d, t))).await??;
    let _ = std::fs::remove_file(&archive);
    let want = m.bundle_version(platform()).unwrap_or(0);
    ensure!(bundle::info(&d).version >= want, "the installed bundle is not the release's (bundle.json says {})", bundle::info(&d).version);
    tracing::info!(dir = %d.display(), version = want, "update: bundle installed");
    Ok(d)
}

/// Downloads the release's client of this kind (`variant`: nxnativev2 for the app, nxnative-cli for the command line) and puts it in place of
/// this executable; it runs from the next start. This app's own bundle is unpacked first: the new executable carries none.
pub async fn install_client(store: &Store, status: &Status, variant: &str, progress: Progress) -> Result<PathBuf> {
    ensure!(release_number() > 0, "a development build is not replaced by a release: build it again");
    let m = &status.manifest;
    let key = format!("{variant}-{}", platform());
    let a = m.client.assets.get(&key).with_context(|| format!("the release has no {key}"))?;
    let store2 = store.clone();
    tokio::task::spawn_blocking(move || bundle::ensure(&store2, &|_, _| {})).await??;
    let exe = std::env::current_exe()?.canonicalize()?;
    let new = fetch(m, &a.tag, &a.file, a.size, &a.sha256, &store.tmp_dir().join("update"), &progress).await?;
    replace_exe(&exe, &new).with_context(|| format!("replace {}", exe.display()))?;
    tracing::info!(exe = %exe.display(), version = %m.client.version, "update: client installed");
    Ok(exe)
}

/// The new executable in place of the running one: Linux renames over it (the running process keeps its file); Windows cannot write a running
/// executable but can rename it, so it moves aside as <name>.old (removed at the next start, cleanup()) and the new one takes its name
fn replace_exe(exe: &Path, new: &Path) -> Result<()> {
    let part = exe.with_extension(if cfg!(windows) { "exe.partial" } else { "partial" });
    std::fs::copy(new, &part)?;
    let _ = std::fs::remove_file(new);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&part, exe)?;
    }
    #[cfg(windows)]
    {
        let old = exe.with_extension("exe.old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old)?;
        if let Err(e) = std::fs::rename(&part, exe) {
            let _ = std::fs::rename(&old, exe);
            return Err(e.into());
        }
    }
    Ok(())
}

/// At start: the executable a Windows update moved aside, and downloads an update left
pub fn cleanup(store: &Store) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
    }
    let _ = std::fs::remove_dir_all(store.tmp_dir().join("update"));
}

#[cfg(test)]
mod tests {
    #[test]
    fn versions() {
        assert!(super::semver("0.2.0") > super::semver("0.1.9"));
        assert!(super::semver("v1.0.0") > super::semver("0.99.99"));
        assert_eq!(super::semver("0.1.0"), (0, 1, 0));
        assert!(super::semver("0.202610251030") > super::semver("0.202610241914"));
        assert!(super::semver("0.202610241914") > super::semver("0.1.0"));   // the first release's numbering, before release numbers
        assert!(super::semver("1.202601010000") > super::semver("0.202612312359"));
    }
    #[test]
    fn changes_after_a_release() {
        let c = |tag: &str| super::Changes { tag: tag.into(), published: String::new(), notes: vec![tag.into()] };
        let m: super::Manifest = serde_json::from_value(serde_json::json!({
            "schema": 1, "tag": "v0.202610251200", "published": "", "commit": "",
            "client": {"version": "0.202610251200", "assets": {}}, "bundle": {"version": 202610251200u64, "min_client": "0.1.0", "assets": {}},
            "changes": [c("v0.202610251200"), c("v0.202610241927"), c("v0.202610242024")]
        })).unwrap();
        assert_eq!(super::tag_number("v0.202610241927"), 202610241927);
        let tags: Vec<String> = super::changes_since(&m, 202610241927).into_iter().map(|c| c.tag).collect();
        assert_eq!(tags, ["v0.202610251200", "v0.202610242024"]);
        assert!(super::changes_since(&m, 202610251200).is_empty());
        // a manifest from before changes were kept reads as none
        let mut old = serde_json::to_value(&m).unwrap();
        old.as_object_mut().unwrap().remove("changes");
        assert!(serde_json::from_value::<super::Manifest>(old).unwrap().changes.is_empty());
    }
    #[test]
    fn replaces_an_executable() {
        let d = std::env::temp_dir().join(format!("nxnativev2-replace-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let (exe, new) = (d.join("app"), d.join("app.download"));
        std::fs::write(&exe, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        super::replace_exe(&exe, &new).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert!(!new.exists());
        std::fs::remove_dir_all(&d).unwrap();
    }
    #[test]
    fn refuses_an_unsigned_manifest() {
        assert!(super::verify(b"{}", &"00".repeat(64)).is_err());
    }
    fn manifest(per_asset: bool) -> super::Manifest {
        serde_json::from_value(serde_json::json!({
            "schema": 1, "tag": "v0.3", "published": "", "commit": "", "per_asset_tags": per_asset,
            "client": {"version": "0.3", "assets": {"nxnativev2-linux-x64": {"file": "a", "sha256": "", "size": 1}}},
            "bundle": {"version": 3, "min_client": "0.1", "assets": {
                "linux-x64": {"file": "b", "sha256": "", "size": 1, "hash": "", "sbom": "", "sbom_sha256": "", "tag": "v0.2", "version": 2},
                "windows-x64": {"file": "c", "sha256": "", "size": 1, "hash": "", "sbom": "", "sbom_sha256": "", "version": 3, "api": 2}}}
        })).unwrap()
    }
    #[test]
    fn assets_come_from_their_release() {
        let m = manifest(true);
        assert_eq!(m.tag_of(&m.bundle.assets["linux-x64"].tag), "v0.2");   // carried over from an older release
        assert_eq!(m.tag_of(&m.bundle.assets["windows-x64"].tag), "v0.3");   // built by this one
        assert_eq!(m.bundle_version("linux-x64"), Some(2));
        assert_eq!(m.bundle.assets["linux-x64"].api, 1);   // a bundle from before the API: 1
        let old = manifest(false);
        assert_eq!(old.tag_of(&old.bundle.assets["linux-x64"].tag), "v0.3");   // without per_asset_tags every asset is in the manifest's release
    }
    #[test]
    fn todays_manifests_still_parse() {
        let m: super::Manifest = serde_json::from_value(serde_json::json!({
            "schema": 1, "tag": "v0.202610241927", "published": "", "commit": "",
            "client": {"version": "0.202610241927", "assets": {}},
            "bundle": {"version": 202610241927u64, "min_client": "0.202610241927", "assets": {}}
        })).unwrap();
        assert!(!m.per_asset_tags);
        assert!(m.client.api.is_none());
    }
    #[test]
    fn hex_round_trip() {
        assert_eq!(super::unhex(&super::hex(&[0, 1, 0xab, 0xff])).unwrap(), vec![0, 1, 0xab, 0xff]);
    }
}
