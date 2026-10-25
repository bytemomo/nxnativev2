//! Games in the data folder: importing dropped files (game files and keys) and listing what each game has. An IPA dropped as is
//! is an app to sideload: an entry of its own, with nothing to build.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::nsp::{self, Kind};
use crate::store::Store;

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf,
    pub kind: Kind,
    pub title_id: u64,
    pub version: Option<u32>,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct Game {
    pub id: u64,
    pub name: String,
    /// The nxnative repository has a game profile for it (games without one get a provisional profile and may not boot)
    pub supported: bool,
    pub files: Vec<FileInfo>,
    pub has_keys: bool,
    pub ipa: Option<PathBuf>,
    /// An iOS app added as an IPA (not a game): installed and copied as it is
    pub app: Option<AppInfo>,
}

/// An app added as an IPA, as its Info.plist names it (`<game dir>/app.json`)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AppInfo {
    pub bundle_id: String,
    pub name: String,
    pub version: Option<String>,
}

impl Game {
    /// The bundle ID its IPA has (before the signing adds the team ID)
    pub fn bundle_id(&self) -> String {
        self.app.as_ref().map(|a| a.bundle_id.clone()).unwrap_or_else(|| bundle_id(self.id))
    }

    pub fn has_base(&self) -> bool {
        self.files.iter().any(|f| f.kind == Kind::Base)
    }

    pub fn newest_update(&self) -> Option<u32> {
        self.files.iter().filter(|f| f.kind == Kind::Update).filter_map(|f| f.version).max()
    }

    pub fn dlc_count(&self) -> usize {
        self.files.iter().filter(|f| f.kind == Kind::Dlc).count()
    }

    /// What is still missing before the IPA can be built
    pub fn missing(&self) -> Vec<&'static str> {
        let mut m = vec![];
        if self.app.is_some() {
            return m;
        }
        if !self.has_base() {
            m.push("the base game");
        }
        if !self.has_keys {
            m.push("prod.keys");
        }
        m
    }
}

/// The name in the nxnative game profile (`dm/games/<id>/game.tsv`, row `name`)
pub fn profile_name(repo: Option<&Path>, id: u64) -> Option<String> {
    let tsv = fs::read_to_string(repo?.join("dm/games").join(format!("{id:016X}")).join("game.tsv")).ok()?;
    tsv.lines().find_map(|l| l.strip_prefix("name\t")).map(|s| s.trim().to_string())
}

pub fn list(store: &Store, repo: Option<&Path>) -> Result<Vec<Game>> {
    let mut games = vec![];
    let Ok(dirs) = fs::read_dir(store.games_dir()) else { return Ok(games) };
    for d in dirs.filter_map(|e| e.ok()) {
        let Ok(id) = u64::from_str_radix(&d.file_name().to_string_lossy(), 16) else { continue };
        games.push(game(store, repo, id)?);
    }
    games.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(games)
}

pub fn game(store: &Store, repo: Option<&Path>, id: u64) -> Result<Game> {
    let ipa = store.ipa(id);
    if let Some(app) = fs::read(app_json(store, id)).ok().and_then(|b| serde_json::from_slice::<AppInfo>(&b).ok()) {
        return Ok(Game { id, name: app.name.clone(), supported: true, files: vec![], has_keys: false, ipa: ipa.is_file().then_some(ipa), app: Some(app) });
    }
    let mut files = vec![];
    if let Ok(rd) = fs::read_dir(store.files_dir(id)) {
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if !nsp::is_game_file(&p) {
                continue;
            }
            if let Ok(f) = nsp::identify(&p) {
                files.push(FileInfo { size: e.metadata()?.len(), path: p, kind: f.kind, title_id: f.title_id, version: f.version });
            }
        }
    }
    files.sort_by_key(|f| (f.kind as u8, f.title_id, f.version));
    let profile = profile_name(repo, id);
    let name = profile
        .clone()
        .or_else(|| files.iter().find(|f| f.kind == Kind::Base).map(|f| nsp::name_from_file(&f.path)))
        .unwrap_or_else(|| format!("{id:016X}"));
    Ok(Game {
        id,
        name,
        supported: profile.is_some(),
        files,
        has_keys: store.prod_keys(id).is_file(),
        ipa: ipa.is_file().then_some(ipa),
        app: None,
    })
}

fn app_json(store: &Store, id: u64) -> PathBuf {
    store.game_dir(id).join("app.json")
}

/// The ID an app's entry has: from its bundle ID, with A as its first digit (Switch title IDs start with 0)
pub fn app_id(bundle_id: &str) -> u64 {
    use sha2::Digest;
    let h = sha2::Sha256::digest(bundle_id.as_bytes());
    u64::from_be_bytes(h[..8].try_into().unwrap()) & 0x0FFF_FFFF_FFFF_FFFF | 0xA000_0000_0000_0000
}

/// The title ID in an IPA this app built before (org.nxnative.<title ID>): dropped again, it is that game's IPA
fn built_here(bundle_id: &str) -> Option<u64> {
    bundle_id.strip_prefix("org.nxnative.").filter(|h| h.len() == 16).and_then(|h| u64::from_str_radix(h, 16).ok())
}

#[derive(Debug, Default)]
pub struct ImportReport {
    /// Games that got files or keys
    pub games: Vec<u64>,
    pub imported: Vec<PathBuf>,
    pub skipped: Vec<(PathBuf, String)>,
}

/// Imports dropped files. Game files go to their game (by title ID); keys files go to every game of this drop, or to `target`
/// when the drop holds no game files; an IPA is an app of its own (or the IPA of the game it was built from here).
/// Files are hard-linked when the data folder is on the same disk, else copied.
/// `progress(bytes done, bytes total)` follows the copying.
pub fn import(store: &Store, paths: &[PathBuf], target: Option<u64>, progress: &dyn Fn(u64, u64)) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    let mut by_game: BTreeMap<u64, Vec<PathBuf>> = BTreeMap::new();
    let mut keys = vec![];
    let mut ipas: Vec<(PathBuf, u64, Option<crate::ipa::Info>)> = vec![];
    for p in expand(paths) {
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        if nsp::is_game_file(&p) {
            match nsp::identify(&p) {
                Ok(f) => by_game.entry(f.app_id).or_default().push(p),
                Err(e) => report.skipped.push((p, e.to_string())),
            }
        } else if name.ends_with(".keys") {
            keys.push(p);
        } else if name.ends_with(".ipa") {
            match crate::ipa::read(&p) {
                Ok(info) => match built_here(&info.bundle_id) {
                    Some(id) => ipas.push((p, id, None)),
                    None => ipas.push((p, app_id(&info.bundle_id), Some(info))),
                },
                Err(e) => report.skipped.push((p, format!("{e:#}"))),
            }
        } else {
            report.skipped.push((p, "not a game file (.nsp / .xci), a keys file (.keys) or an app (.ipa)".into()));
        }
    }
    let key_games: Vec<u64> = if by_game.is_empty() { target.into_iter().collect() } else { by_game.keys().copied().collect() };
    if !keys.is_empty() && key_games.is_empty() {
        bail!("keys dropped without a game: drop them together with the game files, or select a game first");
    }

    let total: u64 = by_game.values().flatten().chain(ipas.iter().map(|i| &i.0)).filter_map(|p| fs::metadata(p).ok()).map(|m| m.len()).sum();
    let mut done = 0u64;
    for (id, files) in &by_game {
        let dir = store.files_dir(*id);
        fs::create_dir_all(&dir)?;
        for src in files {
            let dst = dir.join(src.file_name().unwrap());
            place(src, &dst, &mut |n| {
                done += n;
                progress(done, total)
            })
            .with_context(|| format!("import {}", src.display()))?;
            report.imported.push(src.clone());
        }
    }
    for (src, id, info) in &ipas {
        fs::create_dir_all(store.game_dir(*id))?;
        let dst = store.ipa(*id);
        // a newer IPA of the same app may have the same size: always placed again
        if dst.exists() {
            fs::remove_file(&dst)?;
        }
        place(src, &dst, &mut |n| {
            done += n;
            progress(done, total)
        })
        .with_context(|| format!("import {}", src.display()))?;
        if let Some(info) = info {
            let app = AppInfo { bundle_id: info.bundle_id.clone(), name: info.name.clone(), version: info.version.clone() };
            fs::write(app_json(store, *id), serde_json::to_vec_pretty(&app)?)?;
            let icon = store.game_dir(*id).join("icon.png");
            match &info.icon {
                Some(png) => fs::write(&icon, png)?,
                None => {
                    let _ = fs::remove_file(&icon);
                }
            }
        }
        report.imported.push(src.clone());
    }
    for id in &key_games {
        let dir = store.keys_dir(*id);
        fs::create_dir_all(&dir)?;
        for k in &keys {
            let lower = k.file_name().unwrap().to_string_lossy().to_lowercase();
            let name = if lower.contains("title") { "title.keys" } else { "prod.keys" };
            let dst = dir.join(name);
            fs::copy(k, &dst)?;
            restrict(&dst)?;
            report.imported.push(k.clone());
        }
    }
    tracing::info!(imported = report.imported.len(), skipped = report.skipped.len(), "import");
    report.games = by_game.keys().copied().chain(if keys.is_empty() { vec![] } else { key_games }).chain(ipas.iter().map(|i| i.1)).collect();
    report.games.dedup();
    Ok(report)
}

/// A dropped folder stands for the files in it (not recursive)
fn expand(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = vec![];
    for p in paths {
        if p.is_dir() {
            if let Ok(rd) = fs::read_dir(p) {
                let mut v: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file()).collect();
                v.sort();
                out.extend(v);
            }
        } else {
            out.push(p.clone());
        }
    }
    out
}

/// Puts `src` at `dst`: a hard link when possible (same disk: no space, instant), else a copy reporting its progress
fn place(src: &Path, dst: &Path, progress: &mut dyn FnMut(u64)) -> Result<()> {
    let len = fs::metadata(src)?.len();
    if dst.exists() {
        if fs::metadata(dst)?.len() == len {
            progress(len);
            return Ok(());
        }
        fs::remove_file(dst)?;
    }
    if fs::hard_link(src, dst).is_ok() {
        progress(len);
        return Ok(());
    }
    let tmp = dst.with_extension("partial");
    let mut r = fs::File::open(src)?;
    let mut w = fs::File::create(&tmp)?;
    let mut buf = vec![0u8; 8 << 20];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
        progress(n as u64);
    }
    w.sync_all()?;
    fs::rename(tmp, dst)?;
    Ok(())
}

/// Keys are only readable by the user
fn restrict(p: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(0o600))?;
    }
    let _ = p;
    Ok(())
}

/// Removes a game and everything stored for it
pub fn remove(store: &Store, id: u64) -> Result<()> {
    let d = store.game_dir(id);
    if d.exists() {
        fs::remove_dir_all(d)?;
    }
    Ok(())
}

/// What a finished (or partial) build tells about a game, read from its folder: the icon nx_extract saved, the display version of the
/// newest extracted version and the number of shaders
#[derive(Debug, Clone, Default)]
pub struct BuildFacts {
    pub icon: Option<PathBuf>,
    pub display_version: Option<String>,
    pub shaders: Option<u32>,
}

pub fn build_facts(store: &Store, id: u64) -> BuildFacts {
    let build = store.build_dir(id);
    let mut f = BuildFacts::default();
    // an app added as an IPA: what its Info.plist said when it was added
    if let Some(app) = fs::read(app_json(store, id)).ok().and_then(|b| serde_json::from_slice::<AppInfo>(&b).ok()) {
        f.display_version = app.version;
        f.icon = Some(store.game_dir(id).join("icon.png")).filter(|p| p.is_file());
        return f;
    }
    // the newest extracted version (v<number>)
    let newest = fs::read_dir(build.join("extract"))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_string_lossy().strip_prefix('v').and_then(|v| v.parse::<u64>().ok()).map(|v| (v, e.path())))
        .max_by_key(|(v, _)| *v);
    if let Some((_, dir)) = newest {
        f.icon = Some(dir.join("icon.jpg")).filter(|p| p.is_file());
    }
    // a build without the extracted files kept leaves the icon beside them (build_ipa_from_nsp.py), and its extract logs
    f.icon = f.icon.or_else(|| Some(build.join("icon.jpg")).filter(|p| p.is_file()));
    let newest_log = fs::read_dir(build.join("logs"))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.strip_prefix("extract_v").and_then(|r| r.strip_suffix(".log")).and_then(|v| v.parse::<u64>().ok()).map(|v| (v, e.path()))
        })
        .max_by_key(|(v, _)| *v);
    if let Some((_, log)) = newest_log {
        f.display_version = fs::read_to_string(log)
            .ok()
            .and_then(|t| t.lines().find_map(|l| l.strip_prefix("display version ").map(|s| s.trim().to_string())));
    }
    // else the IPA tells both: its Info.plist's version, and its iPad icon (kept beside the build as icon.png, read once)
    if f.display_version.is_none() || f.icon.is_none() {
        if let Ok(mut zip) = fs::File::open(store.ipa(id)).map_err(anyhow::Error::from).and_then(|f| zip::ZipArchive::new(f).map_err(Into::into)) {
            if f.display_version.is_none() {
                let mut buf = vec![];
                if let Ok(mut e) = zip.by_name("Payload/DmIOS.app/Info.plist") {
                    use std::io::Read;
                    let _ = e.read_to_end(&mut buf);
                }
                f.display_version = plist::from_bytes::<plist::Dictionary>(&buf)
                    .ok()
                    .and_then(|d| d.get("CFBundleShortVersionString").and_then(|v| v.as_string()).map(str::to_string));
            }
            let png = build.join("icon.png");
            if f.icon.is_none() && !png.is_file() {
                if let Ok(mut e) = zip.by_name("Payload/DmIOS.app/AppIcon76x76@2x~ipad.png") {
                    let mut buf = vec![];
                    use std::io::Read;
                    if e.read_to_end(&mut buf).is_ok() {
                        let _ = fs::write(&png, buf);
                    }
                }
            }
            if f.icon.is_none() {
                f.icon = Some(png).filter(|p| p.is_file());
            }
        }
    }
    f.shaders = fs::read_to_string(build.join("logs/dmsh_build.log"))
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("shaders ").and_then(|r| r.split(':').next()).and_then(|n| n.trim().parse().ok())));
    f
}

/// The bundle ID the nxnative script gives a game's app (build_ipa.sh: org.nxnative.<title ID in lower case>)
pub fn bundle_id(id: u64) -> String {
    format!("org.nxnative.{id:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ipa_is_an_app_of_its_own() {
        let dir = std::env::temp_dir().join(format!("nxcore-games-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = Store::new(dir.join("data")).unwrap();
        let (app, built) = (dir.join("Sample.ipa"), dir.join("Built.ipa"));
        crate::ipa::tests::sample_ipa(&app, "com.example.sample");
        crate::ipa::tests::sample_ipa(&built, "org.nxnative.0100152000022000");
        let r = import(&store, &[app, built], None, &|_, _| {}).unwrap();
        assert_eq!(r.imported.len(), 2);
        let id = app_id("com.example.sample");
        assert_eq!(id >> 60, 0xA);
        let list = list(&store, None).unwrap();
        let a = list.iter().find(|g| g.id == id).unwrap();
        assert_eq!((a.name.as_str(), a.bundle_id().as_str()), ("Sample", "com.example.sample"));
        assert!(a.ipa.is_some() && a.missing().is_empty());
        let f = build_facts(&store, id);
        assert_eq!(f.display_version.as_deref(), Some("1.2"));
        assert!(f.icon.is_some());
        // the IPA built here before goes back to its game
        let g = list.iter().find(|g| g.id == 0x0100152000022000).unwrap();
        assert!(g.app.is_none() && g.ipa.is_some());
        fs::remove_dir_all(dir).unwrap();
    }
}
