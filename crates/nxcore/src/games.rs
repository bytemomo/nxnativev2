//! Games in the data folder: importing dropped files (game files and keys) and listing what each game has.

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
}

impl Game {
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
    let ipa = store.ipa(id);
    Ok(Game {
        id,
        name,
        supported: profile.is_some(),
        files,
        has_keys: store.prod_keys(id).is_file(),
        ipa: ipa.is_file().then_some(ipa),
    })
}

#[derive(Debug, Default)]
pub struct ImportReport {
    /// Games that got files or keys
    pub games: Vec<u64>,
    pub imported: Vec<PathBuf>,
    pub skipped: Vec<(PathBuf, String)>,
}

/// Imports dropped files. Game files go to their game (by title ID); keys files go to every game of this drop, or to `target`
/// when the drop holds no game files. Files are hard-linked when the data folder is on the same disk, else copied.
/// `progress(bytes done, bytes total)` follows the copying.
pub fn import(store: &Store, paths: &[PathBuf], target: Option<u64>, progress: &dyn Fn(u64, u64)) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    let mut by_game: BTreeMap<u64, Vec<PathBuf>> = BTreeMap::new();
    let mut keys = vec![];
    for p in expand(paths) {
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_lowercase();
        if nsp::is_game_file(&p) {
            match nsp::identify(&p) {
                Ok(f) => by_game.entry(f.app_id).or_default().push(p),
                Err(e) => report.skipped.push((p, e.to_string())),
            }
        } else if name.ends_with(".keys") {
            keys.push(p);
        } else {
            report.skipped.push((p, "not a game file (.nsp / .xci) or a keys file (.keys)".into()));
        }
    }
    let key_games: Vec<u64> = if by_game.is_empty() { target.into_iter().collect() } else { by_game.keys().copied().collect() };
    if !keys.is_empty() && key_games.is_empty() {
        bail!("keys dropped without a game: drop them together with the game files, or select a game first");
    }

    let total: u64 = by_game.values().flatten().filter_map(|p| fs::metadata(p).ok()).map(|m| m.len()).sum();
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
    report.games = by_game.keys().copied().chain(if keys.is_empty() { vec![] } else { key_games }).collect();
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
