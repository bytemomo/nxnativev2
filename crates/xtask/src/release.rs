//! Releases: what nxcore::update reads. `cargo xtask keygen` makes the release key; `cargo xtask release [--publish] [--swift-object <file>]`
//! builds both platforms' bundles and clients into dist/release/, writes their SBOMs (CycloneDX 1.5), the manifest, and signs it; with
//! --publish it makes the GitHub release (gh). The same command runs on the computer that publishes and in CI (.github/workflows/release.yml).
//!
//! The release key (ed25519): NXNATIVEV2_SIGNING_KEY, the key in hex (CI's secret) or a 1Password reference (op://vault/item/field, read with
//! `op read`), else <config>/nxnativev2/release-signing.key. Its public half is nxcore::update::PUBLIC_KEY: a release is refused when they do
//! not match, as no client would install it (`cargo xtask keycheck` says, without building).

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use nxcore::update::Manifest;
use serde_json::{Value, json};

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn unhex(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    ensure!(s.len().is_multiple_of(2), "hex of odd length");
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("hex")).collect()
}
fn random<const N: usize>() -> Result<[u8; N]> {
    let mut b = [0u8; N];
    fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b)
}
pub fn sha256_file(p: &Path) -> Result<String> {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    let mut f = fs::File::open(p)?;
    let mut buf = vec![0u8; 8 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

/// The release number of a bundle: UTC YYYYMMDDHHMM (NXNATIVEV2_BUNDLE_VERSION sets another)
pub fn bundle_version() -> Result<u64> {
    if let Ok(v) = std::env::var("NXNATIVEV2_BUNDLE_VERSION") {
        return v.parse().context("NXNATIVEV2_BUNDLE_VERSION is a number");
    }
    let (y, mo, d, h, mi, _) = utc(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs());
    Ok(((((y * 100 + mo) * 100 + d) * 100 + h) * 100) + mi)
}
/// Seconds since 1970 → UTC (year, month, day, hour, minute, second): days to civil date (Howard Hinnant's algorithm)
fn utc(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y as u64, m as u64, d as u64, rem / 3600, rem / 60 % 60, rem % 60)
}
fn timestamp() -> String {
    let (y, mo, d, h, mi, s) = utc(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}
fn uuid() -> Result<String> {
    let mut b: [u8; 16] = random()?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    Ok(format!("urn:uuid:{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]))
}

// ---------- the release key ----------
fn key_file() -> Result<PathBuf> {
    Ok(nxcore::settings::project_dirs()?.config_dir().join("release-signing.key"))
}
pub fn keygen() -> Result<()> {
    use ed25519_dalek::SigningKey;
    let path = key_file()?;
    ensure!(!path.exists(), "{} exists already: a new key would make every client refuse the next release (remove it by hand to start over)", path.display());
    let seed: [u8; 32] = random()?;
    let key = SigningKey::from_bytes(&seed);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, hex(&seed) + "\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    eprintln!("[xtask] release key written to {} (keep it private; back it up)", path.display());
    eprintln!("[xtask] its public half, for crates/nxcore/src/update.rs PUBLIC_KEY:");
    println!("{}", hex(key.verifying_key().as_bytes()));
    eprintln!("[xtask] for CI: the private key as the repository secret NXNATIVEV2_SIGNING_KEY (gh secret set NXNATIVEV2_SIGNING_KEY < {})", path.display());
    Ok(())
}
fn signing_key() -> Result<ed25519_dalek::SigningKey> {
    let hexed = match std::env::var("NXNATIVEV2_SIGNING_KEY") {
        // a 1Password reference: the key stays in the vault, `op` reads it for this run
        Ok(v) if v.trim().starts_with("op://") => {
            let out = Command::new("op").args(["read", "--no-newline", v.trim()]).output().context("run 1Password's CLI (op) to read NXNATIVEV2_SIGNING_KEY")?;
            ensure!(out.status.success(), "op read {} failed: {}", v.trim(), String::from_utf8_lossy(&out.stderr).trim());
            String::from_utf8(out.stdout)?
        }
        Ok(v) if !v.trim().is_empty() => v,
        _ => fs::read_to_string(key_file()?).with_context(|| format!("no release key: NXNATIVEV2_SIGNING_KEY (hex or op://...) or {} (cargo xtask keygen)", key_file().unwrap().display()))?,
    };
    let seed: [u8; 32] = unhex(&hexed)?.try_into().map_err(|_| anyhow::anyhow!("the release key is not 32 bytes"))?;
    Ok(ed25519_dalek::SigningKey::from_bytes(&seed))
}

// ---------- SBOMs ----------
fn component(kind: &str, name: &str, version: &str, purl: Option<String>, extra: Value) -> Value {
    let mut c = json!({"type": kind, "name": name, "version": version});
    if let Some(p) = purl {
        c["purl"] = json!(p);
    }
    if let (Some(o), Some(e)) = (c.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            o.insert(k.clone(), v.clone());
        }
    }
    c
}
fn hashed(p: &Path) -> Result<Value> {
    Ok(json!({"hashes": [{"alg": "SHA-256", "content": sha256_file(p)?}]}))
}

/// What a bundle is made of, from its payload folder: the CycloneDX SBOM `bundle-<platform>.cdx.json`
pub fn bundle_sbom(root: &Path, payload: &Path, platform: &str, windows: bool, version: u64, pins: &crate::Pins) -> Result<Value> {
    let mut comps = vec![];
    // nxnative, and the patches on it
    let rev = fs::read_to_string(payload.join("nxnative/.nxnativev2-revision")).unwrap_or_default();
    let (repo, rev) = rev.trim().split_once('\t').unwrap_or(("", ""));
    let mut patches = vec![];
    for dir in ["common", if windows { "windows" } else { "linux" }] {
        let d = root.join("patches/nxnative").join(dir);
        let mut ps: Vec<PathBuf> = fs::read_dir(&d).map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "patch")).collect()).unwrap_or_default();
        ps.sort();
        for p in ps {
            patches.push(json!({"name": format!("patch:{dir}/{}", p.file_name().unwrap().to_string_lossy()), "value": sha256_file(&p)?}));
        }
    }
    comps.push(component("application", "nxnative", rev, Some(format!("pkg:github/{}@{rev}", repo.trim_start_matches("https://github.com/"))),
        json!({"externalReferences": [{"type": "vcs", "url": repo}], "properties": patches, "description": "nxnative with nxnativev2's patches, and nxnativev2's runtime/ as dm/rt"})));
    comps.push(component("application", "cpython", pins.py_version, Some(format!("pkg:generic/python-build-standalone@{}?python={}", pins.py_release, pins.py_version)),
        json!({"externalReferences": [{"type": "distribution", "url": format!("https://github.com/astral-sh/python-build-standalone/releases/tag/{}", pins.py_release)}]})));
    let site = payload.join("python").join(if windows { "Lib/site-packages" } else { "lib/python3.13/site-packages" });
    let mut pkgs: Vec<(String, String)> = vec![];
    for e in fs::read_dir(&site).map(|rd| rd.filter_map(|e| e.ok()).collect::<Vec<_>>()).unwrap_or_default() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !n.ends_with(".dist-info") {
            continue;
        }
        let meta = fs::read_to_string(e.path().join("METADATA")).unwrap_or_default();
        let field = |k: &str| meta.lines().find_map(|l| l.strip_prefix(k)).map(|v| v.trim().to_string());
        if let (Some(name), Some(ver)) = (field("Name:"), field("Version:")) {
            pkgs.push((name, ver));
        }
    }
    pkgs.sort();
    for (name, ver) in pkgs {
        comps.push(component("library", &name, &ver, Some(format!("pkg:pypi/{}@{ver}", name.to_lowercase())), json!({})));
    }
    comps.push(component("framework", "Microsoft.NETCore.App", pins.dotnet, Some(format!("pkg:nuget/Microsoft.NETCore.App.Runtime.{}@{}", if windows { "win-x64" } else { "linux-x64" }, pins.dotnet)), json!({})));
    let nx = payload.join("prebuilt/nx_extract").join(if windows { "nx_extract.exe" } else { "nx_extract" });
    if nx.is_file() {
        comps.push(component("application", "nx_extract", rev, None, hashed(&nx)?));
    }
    comps.push(component("application", "spirv-cross", pins.spirv_cross, Some(format!("pkg:github/KhronosGroup/SPIRV-Cross@{}", pins.spirv_cross)), json!({})));
    comps.push(component("application", "llvm", pins.llvm, Some(format!("pkg:github/llvm/llvm-project@llvmorg-{}", pins.llvm)), json!({"description": "clang and lld with clang's headers"})));
    for f in ["DejaVuSans.ttf", "DejaVuSans-Bold.ttf"] {
        let p = payload.join("fonts").join(f);
        if p.is_file() {
            comps.push(component("file", f, "", None, hashed(&p)?));
        }
    }
    let swift = payload.join("prebuilt/swift_DmIOS.o");
    if swift.is_file() {
        let mut c = component("file", "swift_DmIOS.o", "", None, hashed(&swift)?);
        c["description"] = json!("the iOS app's Swift sources, compiled for arm64 iOS");
        comps.push(c);
    }
    for f in ["bin/xcrun.exe", "bin/dotnet-dll-launcher.exe"] {
        let p = payload.join(f);
        if p.is_file() {
            comps.push(component("application", f.trim_start_matches("bin/"), env!("CARGO_PKG_VERSION"), None, hashed(&p)?));
        }
    }
    // the whole tree: every file's path and SHA-256, sorted, hashed once more
    let (mut lines, mut stack, mut total) = (vec![], vec![payload.to_path_buf()], 0u64);
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d)?.filter_map(|e| e.ok()) {
            let (p, t) = (e.path(), e.file_type()?);
            if t.is_dir() {
                stack.push(p);
            } else if t.is_file() {
                total += e.metadata()?.len();
                lines.push(format!("{}\t{}", p.strip_prefix(payload)?.to_string_lossy().replace('\\', "/"), sha256_file(&p)?));
            }
        }
    }
    lines.sort();
    let tree = {
        use sha2::Digest;
        hex(&sha2::Sha256::digest(lines.join("\n").as_bytes()))
    };
    Ok(json!({
        "bomFormat": "CycloneDX", "specVersion": "1.5", "serialNumber": uuid()?, "version": 1,
        "metadata": {
            "timestamp": timestamp(),
            "tools": {"components": [{"type": "application", "name": "nxnativev2 xtask", "version": env!("CARGO_PKG_VERSION")}]},
            "component": {"type": "application", "name": format!("nxnativev2-bundle-{platform}"), "version": version.to_string(), "bom-ref": "bundle"},
            "properties": [
                {"name": "nxnativev2:platform", "value": platform},
                {"name": "nxnativev2:files", "value": lines.len().to_string()},
                {"name": "nxnativev2:bytes", "value": total.to_string()},
                {"name": "nxnativev2:tree-sha256", "value": tree}
            ]
        },
        "components": comps
    }))
}

/// The client's SBOM: the workspace's Rust crates (cargo metadata)
fn client_sbom(root: &Path, version: &str) -> Result<Value> {
    let out = Command::new("cargo").args(["metadata", "--format-version", "1", "--locked"]).current_dir(root).output()?;
    ensure!(out.status.success(), "cargo metadata failed: {}", String::from_utf8_lossy(&out.stderr));
    let meta: Value = serde_json::from_slice(&out.stdout)?;
    let members: Vec<&str> = meta["workspace_members"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
    let mut comps = vec![];
    for p in meta["packages"].as_array().into_iter().flatten() {
        if members.contains(&p["id"].as_str().unwrap_or("")) {
            continue;
        }
        let (name, ver) = (p["name"].as_str().unwrap_or(""), p["version"].as_str().unwrap_or(""));
        let mut c = component("library", name, ver, Some(format!("pkg:cargo/{name}@{ver}")), json!({}));
        if let Some(l) = p["license"].as_str() {
            c["licenses"] = json!([{"expression": l}]);
        }
        comps.push(c);
    }
    comps.sort_by(|a, b| (a["name"].as_str(), a["version"].as_str()).cmp(&(b["name"].as_str(), b["version"].as_str())));
    Ok(json!({
        "bomFormat": "CycloneDX", "specVersion": "1.5", "serialNumber": uuid()?, "version": 1,
        "metadata": {"timestamp": timestamp(), "component": {"type": "application", "name": "nxnativev2", "version": version}},
        "components": comps
    }))
}

// ---------- the release ----------
/// What this release is the first to build: the subjects of the commits after the previous release's (without [ci]), newest first;
/// the last commit's alone for a first release, or when the previous one's commit is not in this checkout's history
fn changes_since(root: &Path, prev: Option<&Manifest>) -> Vec<String> {
    let known = prev.filter(|m| !m.commit.is_empty() && !git(root, &["cat-file", "-t", &m.commit]).is_empty());
    let log = match known {
        Some(m) => git(root, &["log", "--no-merges", "--format=%s", &format!("{}..HEAD", m.commit)]),
        None => git(root, &["log", "-1", "--format=%s"]),
    };
    log.lines().map(|l| l.trim().trim_end_matches("[ci]").trim_end().to_string()).filter(|l| !l.is_empty()).collect()
}

fn git(root: &Path, args: &[&str]) -> String {
    Command::new("git").args(args).current_dir(root).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

/// The release key in reach matches the client's built-in public key (nothing is built)
pub fn keycheck() -> Result<()> {
    let key = signing_key()?;
    let public = hex(key.verifying_key().as_bytes());
    ensure!(public == nxcore::update::PUBLIC_KEY, "the release key's public half ({public}) is not nxcore::update::PUBLIC_KEY: no client would install what it signs");
    crate::say(format!("the release key matches the client's ({public})"));
    Ok(())
}

// ---------- what a release builds: each component's inputs ----------
const PLATFORMS: [(&str, bool); 2] = [("linux-x64", false), ("windows-x64", true)];

/// A hash of the files under `paths` git knows or would add (their contents in the working tree: uncommitted changes and new files
/// included, ignored ones not) and `extra`
fn inputs(root: &Path, paths: &[&str], extra: &str) -> Result<String> {
    use sha2::Digest;
    let out = Command::new("git").args(["ls-files", "-z", "--cached", "--others", "--exclude-standard", "--"]).args(paths).current_dir(root).output()?;
    ensure!(out.status.success(), "git ls-files failed: {}", String::from_utf8_lossy(&out.stderr));
    let mut files: Vec<String> = out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).collect();
    files.sort();
    let mut h = sha2::Sha256::new();
    h.update(extra.as_bytes());
    for f in &files {
        h.update(f.as_bytes());
        h.update([0]);
        match fs::read(root.join(f)) {
            Ok(b) => h.update(&b),
            Err(_) => h.update(b"\0deleted"),
        }
        h.update([0]);
    }
    Ok(hex(&h.finalize()))
}
/// A platform's bundle: nxnative's revision and patches, the runtime, how xtask builds it (its pinned tools), its API; Windows' launchers
fn bundle_inputs(root: &Path, platform: &str) -> Result<String> {
    let windows = platform == "windows-x64";
    let mut paths = vec!["patches/nxnative/UPSTREAM", "patches/nxnative/common", "runtime", "crates/codecs", "crates/xtask/src/main.rs"];
    paths.push(if windows { "patches/nxnative/windows" } else { "patches/nxnative/linux" });
    if windows {
        paths.push("crates/launchers");
    }
    inputs(root, &paths, &format!("bundle {platform} api {}", nxcore::update::BUNDLE_API))
}
/// The app and the command line (both platforms: one set of sources)
fn client_inputs(root: &Path) -> Result<String> {
    inputs(root, &["crates/nxcore", "crates/nxapp", "crates/nxcli", "Cargo.toml", "Cargo.lock"], &format!("client major {}", env!("CARGO_PKG_VERSION_MAJOR")))
}

fn http() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder().user_agent(concat!("nxnativev2-xtask/", env!("CARGO_PKG_VERSION"))).timeout(None).build()?)
}

/// The latest release's manifest, checked against the release key; None when there is no release yet
fn latest() -> Result<Option<Manifest>> {
    let base = format!("{}/latest/download", nxcore::update::releases());
    let c = http()?;
    let get = |f: &str| -> Result<Option<Vec<u8>>> {
        let r = c.get(format!("{base}/{f}")).send()?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(r.error_for_status()?.bytes()?.to_vec()))
    };
    let Some(raw) = get("manifest.json")? else { return Ok(None) };
    let sig = get("manifest.json.sig")?.context("the latest release has a manifest but no signature")?;
    nxcore::update::verify(&raw, &String::from_utf8(sig)?)?;
    Ok(Some(serde_json::from_slice(&raw).context("the latest release's manifest")?))
}

/// An asset of an earlier release (one this release carries over), verified against its manifest; cached in target/release-cache by SHA-256
fn fetch_prev(root: &Path, m: &Manifest, tag: &Option<String>, file: &str, sha256: &str, size: u64) -> Result<PathBuf> {
    let cache = root.join("target/release-cache");
    fs::create_dir_all(&cache)?;
    let out = cache.join(sha256);
    if out.is_file() && fs::metadata(&out)?.len() == size && sha256_file(&out)? == sha256 {
        return Ok(out);
    }
    let from = m.tag_of(tag);
    let url = format!("{}/download/{from}/{file}", nxcore::update::releases());
    crate::say(format!("{file}: unchanged, downloading it from {from}"));
    let mut r = http()?.get(&url).send()?.error_for_status().with_context(|| format!("download {url}"))?;
    let tmp = out.with_extension("partial");
    let mut f = fs::File::create(&tmp)?;
    std::io::copy(&mut r, &mut f)?;
    drop(f);
    ensure!(fs::metadata(&tmp)?.len() == size && sha256_file(&tmp)? == sha256, "{file} from {from} does not match its manifest");
    fs::rename(&tmp, &out)?;
    Ok(out)
}

/// What a release builds, and why
pub struct Plan {
    pub bundles: Vec<&'static str>,
    pub client: bool,
    pub reasons: Vec<String>,
    bundle_inputs: BTreeMap<&'static str, String>,
    client_inputs: String,
}
fn plan(root: &Path, prev: &Option<Manifest>) -> Result<Plan> {
    let (mut bundles, mut reasons, mut binputs) = (vec![], vec![], BTreeMap::new());
    for (platform, _) in PLATFORMS {
        let now = bundle_inputs(root, platform)?;
        let was = prev.as_ref().and_then(|m| m.bundle.assets.get(platform).map(|a| (a.inputs.clone(), m.tag_of(&a.tag), a.version.unwrap_or(m.bundle.version))));
        match &was {
            Some((Some(i), tag, v)) if *i == now => reasons.push(format!("bundle {platform}: unchanged (version {v}, in {tag})")),
            Some((Some(_), _, _)) => { bundles.push(platform); reasons.push(format!("bundle {platform}: its inputs changed: new")); }
            _ => { bundles.push(platform); reasons.push(format!("bundle {platform}: none published with its inputs yet: new")); }
        }
        binputs.insert(platform, now);
    }
    let now = client_inputs(root)?;
    let client = match prev {
        Some(m) if m.per_asset_tags && m.client.inputs.as_deref() == Some(now.as_str()) => {
            reasons.push(format!("app: unchanged (version {})", m.client.version));
            false
        }
        Some(m) if !m.per_asset_tags => { reasons.push("app: the latest release is from before incremental releases: new (every app updates through it)".into()); true }
        Some(_) => { reasons.push("app: its inputs changed: new".into()); true }
        None => { reasons.push("app: no release yet: new".into()); true }
    };
    Ok(Plan { bundles, client, reasons, bundle_inputs: binputs, client_inputs: now })
}

pub fn release(args: &[String]) -> Result<()> {
    use nxcore::update::{Asset, BUNDLE_API, BUNDLE_API_MIN, BundleAsset, BundleRelease, ClientRelease};
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
    let publish = args.iter().any(|a| a == "--publish");
    let swift = crate::arg(args, "--swift-object").map(str::to_string);
    let prev = latest()?;
    let plan = plan(&root, &prev)?;
    for r in &plan.reasons {
        crate::say(r);
    }
    if args.iter().any(|a| a == "--plan") {
        // CI: what to build, for the next jobs (the macOS one compiles the Swift object only when a bundle is built)
        if let Ok(out) = std::env::var("GITHUB_OUTPUT") {
            let build = !plan.bundles.is_empty() || plan.client;
            fs::OpenOptions::new().append(true).create(true).open(out)?
                .write_all(format!("bundles={}\nclient={}\nbuild={build}\n", plan.bundles.join(","), plan.client).as_bytes())?;
        }
        return Ok(());
    }
    if plan.bundles.is_empty() && !plan.client {
        crate::say(format!("nothing changed since {}: no release", prev.as_ref().map_or("-".into(), |m| m.tag.clone())));
        return Ok(());
    }
    let key = signing_key()?;
    ensure!(hex(key.verifying_key().as_bytes()) == nxcore::update::PUBLIC_KEY,
        "the release key's public half is not nxcore::update::PUBLIC_KEY: no client would install this release (cargo xtask keycheck says which it is)");
    if publish {
        // a release names its commit: GitHub refuses one it does not have (422, target_commitish is invalid)
        let head = git(&root, &["rev-parse", "HEAD"]);
        let _ = Command::new("git").args(["fetch", "-q", "origin"]).current_dir(&root).status();
        ensure!(!git(&root, &["branch", "-r", "--contains", "HEAD"]).is_empty(),
            "commit {} is not on GitHub yet: push it first (the release names the commit it was built from)", &head[..head.len().min(12)]);
        if !git(&root, &["status", "--porcelain", "--untracked-files=no"]).is_empty() {
            crate::say("note: uncommitted changes go into this release, but not into the commit it names");
        }
    }
    let version = bundle_version()?;
    let client_version = format!("{}.{version}", env!("CARGO_PKG_VERSION_MAJOR"));   // nxcore::update::client_version of the apps built here
    let tag = format!("v{client_version}");
    // dist/ (NXNATIVEV2_DIST_DIR sets another: tests of the release flow): the full apps, and the release's files in release/
    let dist = std::env::var_os("NXNATIVEV2_DIST_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("dist"));
    let dir = dist.join("release");
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;
    crate::say(format!("release {tag}: {}{}", if plan.bundles.is_empty() { String::new() } else { format!("bundles {} ", plan.bundles.join(" ")) }, if plan.client { "and the app" } else { "(the app carried over)" }));
    let asset = |file: &str, tag: &str, version: Option<String>| -> Result<Asset> {
        let p = dir.join(file);
        Ok(Asset { file: file.into(), sha256: sha256_file(&p)?, size: fs::metadata(&p)?.len(), tag: Some(tag.into()), version })
    };
    let sdk = crate::sdk_dir(args)?;
    let (mut bundles, mut clients, mut installers) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    for (platform, windows) in PLATFORMS {
        let ctx = crate::Ctx::new(&root, windows, version);
        let ext = ctx.ext();
        // this platform's bundle: built here, or carried over from the release that has it
        let mut archive = None;   // (path, folder name, length, version): what the full app packs
        if plan.bundles.contains(&platform) {
            crate::payload(&ctx, swift.as_deref(), &sdk)?;
            let sbom_file = format!("bundle-{platform}.cdx.json");
            let sbom = bundle_sbom(&root, &ctx.payload, platform, windows, version, &crate::PINS)?;
            fs::write(dir.join(&sbom_file), serde_json::to_vec_pretty(&sbom)?)?;
            crate::say(format!("SBOM: {sbom_file} ({} components)", sbom["components"].as_array().map_or(0, |a| a.len())));
            let (arc, hash, len) = crate::archive(&ctx)?;
            let file = format!("bundle-{platform}.tar.zst");
            fs::copy(&arc, dir.join(&file))?;
            let a = asset(&file, &tag, None)?;
            bundles.insert(platform.to_string(), BundleAsset {
                file, sha256: a.sha256, size: a.size, hash: hash.clone(), sbom: sbom_file.clone(), sbom_sha256: sha256_file(&dir.join(&sbom_file))?,
                tag: Some(tag.clone()), version: Some(version), api: BUNDLE_API, inputs: Some(plan.bundle_inputs[platform].clone()),
            });
            archive = Some((arc, hash, len, version));
        } else {
            let m = prev.as_ref().context("a carried-over bundle needs a previous release")?;
            let mut a = m.bundle.assets.get(platform).cloned().context("the previous release has no bundle for this platform")?;
            a.tag = Some(m.tag_of(&a.tag));
            a.version = Some(a.version.unwrap_or(m.bundle.version));
            if plan.client {
                archive = Some((fetch_prev(&root, m, &a.tag, &a.file, &a.sha256, a.size)?, a.hash.clone(), a.size, a.version.unwrap()));
            }
            bundles.insert(platform.to_string(), a);
        }
        // the app and the command line: built here (and the full app packed with this platform's newest bundle), or carried over
        if plan.client {
            let (arc, hash, len, bver) = archive.as_ref().unwrap();
            for (variant, exe) in crate::clients(&ctx, true)? {
                let file = format!("{variant}-{platform}-client{ext}");
                fs::copy(&exe, dir.join(&file))?;
                clients.insert(format!("{variant}-{platform}"), asset(&file, &tag, Some(client_version.clone()))?);
                let full = dist.join(format!("{variant}-{platform}{ext}"));
                crate::pack(&exe, arc, *len, *bver, hash, &full)?;
                if variant == "nxnativev2" {
                    let f = format!("nxnativev2-{platform}{ext}");
                    fs::copy(&full, dir.join(&f))?;
                    installers.insert(platform.to_string(), asset(&f, &tag, Some(client_version.clone()))?);
                }
            }
        } else {
            // apps from before incremental releases fetch everything from the manifest's own release: the latest app's files go into every one
            let m = prev.as_ref().unwrap();
            for variant in ["nxnativev2", "nxnative-cli"] {
                let k = format!("{variant}-{platform}");
                let mut a = m.client.assets.get(&k).cloned().with_context(|| format!("the previous release has no {k}"))?;
                fs::copy(fetch_prev(&root, m, &a.tag, &a.file, &a.sha256, a.size)?, dir.join(&a.file))?;
                a.version = Some(a.version.clone().unwrap_or_else(|| m.client.version.clone()));
                a.tag = Some(tag.clone());
                clients.insert(k, a);
            }
            if let Some(i) = m.installers.get(platform) {
                let mut i = i.clone();
                i.tag = Some(m.tag_of(&i.tag));
                installers.insert(platform.to_string(), i);
            }
        }
        crate::clean(&ctx);
    }
    if plan.client {
        fs::write(dir.join("nxnativev2-client.cdx.json"), serde_json::to_vec_pretty(&client_sbom(&root, &client_version)?)?)?;
    }
    let m = prev.as_ref();
    let carried = !plan.client;
    let published = timestamp();
    let mut changes = vec![nxcore::update::Changes { tag: tag.clone(), published: published.clone(), notes: changes_since(&root, m) }];
    changes.extend(m.map(|m| m.changes.clone()).unwrap_or_default());
    changes.truncate(nxcore::update::CHANGES_KEPT);
    let manifest = Manifest {
        schema: 1,
        tag: tag.clone(),
        published,
        commit: git(&root, &["rev-parse", "HEAD"]),
        client: ClientRelease {
            version: if carried { m.unwrap().client.version.clone() } else { client_version.clone() },
            assets: clients,
            api: if carried { m.unwrap().client.api } else { Some([BUNDLE_API_MIN, BUNDLE_API]) },
            inputs: Some(if carried { m.unwrap().client.inputs.clone().unwrap_or_default() } else { plan.client_inputs.clone() }),
        },
        bundle: BundleRelease {
            version: bundles.values().filter_map(|b: &BundleAsset| b.version).max().unwrap_or(version),
            // the first app that reads per-asset tags: an older one updates itself (from this release) before it takes a bundle
            min_client: match m {
                Some(m) if m.per_asset_tags => m.bundle.min_client.clone(),
                _ => client_version.clone(),
            },
            assets: bundles,
        },
        per_asset_tags: true,
        installers,
        changes,
    };
    let raw = serde_json::to_vec_pretty(&manifest)?;
    // the manifest's signature, checked here as a client checks it
    use ed25519_dalek::Signer;
    let sig = hex(&key.sign(&raw).to_bytes());
    nxcore::update::verify(&raw, &sig)?;
    fs::write(dir.join("manifest.json"), &raw)?;
    fs::write(dir.join("manifest.json.sig"), &sig)?;
    crate::say(format!("release {tag}: manifest signed ({} files)", fs::read_dir(&dir)?.count()));
    if !publish {
        crate::say("not published (--publish makes the GitHub release)");
        return Ok(());
    }
    let mut notes = vec![];
    notes.push(if carried { format!("- App {} (unchanged)", manifest.client.version) } else { format!("- App {client_version} (new)") });
    for (p, b) in &manifest.bundle.assets {
        notes.push(format!("- Bundle {p} {} ({})", b.version.unwrap_or(0), if b.tag.as_deref() == Some(tag.as_str()) { "new" } else { "unchanged" }));
    }
    for (p, i) in &manifest.installers {
        notes.push(format!("- First install on {p}: [{}](https://github.com/{}/releases/download/{}/{})", i.file, nxcore::update::REPO, i.tag.clone().unwrap_or_default(), i.file));
    }
    let new: Vec<String> = manifest.changes[0].notes.iter().map(|n| format!("- {n}")).collect();
    let notes = format!("What's new:\n{}\n\n{}\n\nCommit {}\n\nThe app checks the signed manifest.json before it installs anything; each bundle's SBOM (CycloneDX) is bundle-<platform>.cdx.json.", new.join("\n"), notes.join("\n"), manifest.commit);
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file()).collect();
    files.sort();
    let mut cmd = Command::new("gh");
    cmd.args(["release", "create", &tag, "--repo", nxcore::update::REPO, "--title", &format!("nxnativev2 {tag}"), "--notes", &notes, "--latest"]);
    if !manifest.commit.is_empty() {
        cmd.args(["--target", &manifest.commit]);
    }
    cmd.args(&files);
    let st = cmd.status().context("run gh (GitHub's CLI) to make the release")?;
    if !st.success() {
        bail!("gh release create failed ({st})");
    }
    crate::say(format!("published {tag} on {}", nxcore::update::REPO));
    Ok(())
}
