//! Building the IPA: runs the nxnative script (`scripts/build_ipa_from_nsp.py --progress json`) on a game of the data folder and
//! turns its JSON events into [`Event`]s. The script's work folder (`<NSP folder>/<id>_build`) is linked to the game's `build/`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::bundle;
use crate::metal::{self, which};
use crate::settings::Settings;
use crate::store::Store;

#[derive(Debug, Clone)]
pub enum Event {
    /// A stage starts: k of n
    Stage { k: u32, n: u32, name: String },
    Progress { name: String, step: String, frac: Option<f64>, count: String, eta: Option<f64> },
    StageEnd { name: String, ok: bool, summary: String },
    /// A message of the script ([RED] lines are problems)
    Say(String),
    /// A line the script wrote outside its JSON events (Python tracebacks)
    Log(String),
}

/// The unpacked bundle of a packaged app (None for a development build, or before the first unpack)
fn bundled(store: &Store) -> Option<bundle::Paths> {
    bundle::ready(store).map(|d| bundle::Paths::new(&d))
}

/// The nxnative checkout: the one set in the settings (a developer's own), else the patched one the packaged app carries
pub fn repo(settings: &Settings, store: &Store) -> Option<PathBuf> {
    match &settings.nxnative_repo {
        Some(r) => Some(r.clone()).filter(|r| r.join("scripts/build_ipa_from_nsp.py").is_file()),
        None => bundled(store).map(|b| b.nxnative()),
    }
}

/// The Python that runs the scripts: the one set in the settings, else the bundled one, else the checkout's .venv, else the system's
pub fn python(settings: &Settings, store: &Store) -> PathBuf {
    if let Some(p) = &settings.python {
        return p.clone();
    }
    if let Some(b) = bundled(store) {
        return b.python();
    }
    if let Some(r) = repo(settings, store) {
        let venv = if cfg!(windows) { r.join(".venv/Scripts/python.exe") } else { r.join(".venv/bin/python") };
        if venv.is_file() {
            return venv;
        }
    }
    PathBuf::from(if cfg!(windows) { "python" } else { "python3" })
}

#[derive(Debug, Clone)]
pub struct Requirement {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// Everything the build needs, each with what is wrong when it is missing. A packaged app brings its own tools; what remains is Apple's
/// (the user's downloads) and, on Linux, wine, make and bash.
pub fn check(settings: &Settings, store: &Store) -> Vec<Requirement> {
    let mut v = vec![];
    let mut req = |name, ok: bool, detail: String| v.push(Requirement { name, ok, detail });
    let packaged = bundle::locate().is_some();
    let b = bundled(store);
    if packaged {
        // not an error before the first build: it is unpacked then
        req("App bundle", true, b.as_ref().map_or("in the app; unpacked on the first build".into(), |b| b.root.display().to_string()));
    }
    let repo = repo(settings, store);
    if !packaged || settings.nxnative_repo.is_some() {
        req("nxnative scripts", repo.is_some(), repo.as_ref().map_or("set nxnative_repo in the settings (a development build carries no bundle)".into(), |r| r.display().to_string()));
    }
    // a packaged app carries Python and the tools below (unpacked on the first build)
    if (!packaged && b.is_none()) || settings.python.is_some() {
        let py = python(settings, store);
        let mods = Command::new(&py)
            .args(["-c", "import lz4.block, capstone, fontTools, PIL\ntry:\n import compression.zstd\nexcept ImportError:\n import backports.zstd"])
            .output();
        req(
            "Python with the pipeline's modules",
            matches!(&mods, Ok(o) if o.status.success()),
            match &mods {
                Ok(o) if o.status.success() => py.display().to_string(),
                Ok(o) => format!("{}: {}", py.display(), String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or_default()),
                Err(e) => format!("{}: {e}", py.display()),
            },
        );
    }
    let mut tools: Vec<(&'static str, &'static str)> = vec![];
    if !packaged && b.is_none() {
        tools.extend([("dotnet", ".NET SDK 10"), ("spirv-cross", "spirv-cross"), ("clang", "clang (LLVM)"), ("ld64.lld", "lld (LLVM)")]);
    }
    if !cfg!(windows) {
        tools.extend([("make", "make"), ("bash", "bash"), ("wine", "wine (runs Apple's Metal compiler)")]);
    }
    for (t, pkg) in tools {
        let p = which(t);
        req(t, p.is_some(), p.map_or(format!("not found: install {pkg}"), |p| p.display().to_string()));
    }
    let sdk = store.sdk();
    req("iPhoneOS SDK", sdk.is_some(), sdk.map_or("add Xcode.xip".into(), |s| s.display().to_string()));
    // the app's Swift sources (nxnative's launcher screen): prebuilt in a packaged app, else swiftc with Xcode's Swift resources
    let swift_sources = repo.as_ref().and_then(|r| std::fs::read_dir(r.join("dm/ios/Sources")).ok()).is_some_and(|rd| rd.filter_map(|e| e.ok()).any(|e| e.path().extension().is_some_and(|x| x == "swift")));
    let prebuilt = packaged || b.as_ref().is_some_and(|b| b.swift_object().is_file());
    if swift_sources && !prebuilt {
        let sc = which("swiftc");
        req("swiftc", sc.is_some(), sc.map_or("not found: install Swift (swift.org)".into(), |p| p.display().to_string()));
        let res = store.swift_resources();
        req("Swift resources (from Xcode)", res.is_some(), res.map_or("add Xcode.xip again (it also gives Xcode's Swift resources)".into(), |p| p.display().to_string()));
    }
    let metal = metal::installed(store);
    req("Metal compiler", metal, if metal { metal::tools_dir(store).display().to_string() } else { "add the Metal Developer Tools for Windows installer".into() });
    v
}

pub struct BuildHandle {
    child: Arc<Mutex<Child>>,
    reader: Option<std::thread::JoinHandle<Option<String>>>,
}

/// Stops a running build from another thread
#[derive(Clone)]
pub struct Canceller(Arc<Mutex<Child>>);

impl Canceller {
    pub fn cancel(&self) {
        let _ = self.0.lock().unwrap().kill();
    }
}

impl BuildHandle {
    pub fn canceller(&self) -> Canceller {
        Canceller(self.child.clone())
    }

    /// Waits for the end: Ok, or the error the script reported last
    pub fn wait(mut self) -> Result<()> {
        let last_red = self.reader.take().and_then(|h| h.join().ok()).flatten();
        let status = loop {
            // polled, so that the Canceller can take the lock to kill the process meanwhile
            if let Some(s) = self.child.lock().unwrap().try_wait()? {
                break s;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        if status.success() {
            return Ok(());
        }
        bail!("{}", last_red.unwrap_or_else(|| format!("the build failed ({status})")))
    }
}

/// Unpacks the packaged app's bundle the first time (nothing to do for a development build). `progress(bytes, total, step)`
pub async fn prepare(_settings: &Settings, store: &Store, progress: &(dyn Fn(u64, u64, &str) + Sync)) -> Result<()> {
    let store = store.clone();
    let p = |d: u64, t: u64| progress(d, t, "unpacking the app's build tools");
    // the unpack is blocking work; the progress callback is not Send, so it runs here on a blocking section of this task
    tokio::task::block_in_place(|| bundle::ensure(&store, &p))?;
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Game language code (empty = English)
    pub language: Option<String>,
    pub show_fps: bool,
}

/// Starts building the IPA of game `id`; `on_event` is called from a reader thread
pub fn start(settings: &Settings, store: &Store, id: u64, opts: &Options, on_event: impl Fn(Event) + Send + Sync + 'static) -> Result<BuildHandle> {
    let repo = repo(settings, store).context("no nxnative scripts: the app's bundle is not unpacked (prepare first), or a development build without nxnative_repo")?;
    let files = store.files_dir(id);
    let build = store.build_dir(id);
    std::fs::create_dir_all(&build)?;
    link_dir(&build, &files.join(format!("{id:016X}_build")))?;
    let sdk = store.sdk().context("no iPhoneOS SDK: add Xcode.xip first")?;

    let mut cmd = Command::new(python(settings, store));
    cmd.arg(repo.join("scripts/build_ipa_from_nsp.py"))
        .arg(&files)
        .arg("-k")
        .arg(store.prod_keys(id))
        .arg("-o")
        .arg(store.ipa(id))
        .args(["--progress", "json"])
        .arg(if opts.show_fps { "--show-fps" } else { "--no-show-fps" })
        .current_dir(&repo)
        .env("DM_IOS_SDK", &sdk)
        .env("DM_SWIFT_RESOURCES", store.swift_resources().unwrap_or_default())
        .env("TMPDIR", store.tmp_dir())
        .env("TEMP", store.tmp_dir())
        .env("TMP", store.tmp_dir())
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(l) = &opts.language {
        cmd.arg("--language").arg(l);
    }
    let bundle_env = bundled(store).map(|b| b.env()).unwrap_or_default();
    for (k, v) in metal::env(store).into_iter().chain(bundle_env) {
        cmd.env(k, v);
    }
    std::fs::create_dir_all(store.tmp_dir())?;
    tracing::info!(game = %format!("{id:016X}"), repo = %repo.display(), "build: starting the nxnative script");
    let mut child = cmd.spawn().context("start the nxnative script")?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let on_event = Arc::new(on_event);
    let err_events = on_event.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            tracing::info!("build | {line}");
            err_events(Event::Log(line));
        }
    });
    let reader = std::thread::spawn(move || {
        let mut last_red = None;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            match parse(&line) {
                Some(Event::Stage { k, n, name }) => {
                    tracing::info!("build: [{k}/{n}] {name}");
                    on_event(Event::Stage { k, n, name });
                }
                Some(Event::StageEnd { name, ok, summary }) => {
                    if ok { tracing::info!("build: done {name}: {summary}") } else { tracing::error!("build: failed {name}: {summary}") }
                    on_event(Event::StageEnd { name, ok, summary });
                }
                Some(Event::Say(m)) => {
                    tracing::info!("build: {m}");
                    if let Some(r) = m.strip_prefix("[RED] ") {
                        last_red = Some(r.to_string());
                    }
                    on_event(Event::Say(m));
                }
                Some(e) => on_event(e),
                None => on_event(Event::Log(line)),
            }
        }
        last_red
    });
    Ok(BuildHandle { child: Arc::new(Mutex::new(child)), reader: Some(reader) })
}

fn parse(line: &str) -> Option<Event> {
    let v: Value = serde_json::from_str(line).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let u = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or_default() as u32;
    Some(match v.get("ev")?.as_str()? {
        "stage" => Event::Stage { k: u("k"), n: u("n"), name: s("name") },
        "step" => Event::Progress { name: s("name"), step: s("step"), frac: None, count: String::new(), eta: None },
        "progress" => {
            let done = v.get("done").and_then(Value::as_f64);
            let total = v.get("total").and_then(Value::as_f64);
            let unit = s("unit");
            let count = match (done, total) {
                (Some(d), Some(t)) if unit == "B" => format!("{:.1}/{:.1} GB", d / 1e9, t / 1e9),
                (Some(d), Some(t)) => format!("{}/{} {unit}", d as u64, t as u64),
                (Some(d), None) => format!("{} {unit}", d as u64),
                _ => String::new(),
            };
            Event::Progress {
                name: s("name"),
                step: s("step"),
                frac: v.get("frac").and_then(Value::as_f64),
                count,
                eta: v.get("eta").and_then(Value::as_f64),
            }
        }
        "stage_end" => Event::StageEnd { name: s("name"), ok: v.get("ok").and_then(Value::as_bool).unwrap_or(true), summary: s("summary") },
        "say" => Event::Say(s("msg")),
        _ => return None,
    })
}

/// `link` → `target` (a symlink on Unix; a junction on Windows, which needs no privileges)
fn link_dir(target: &Path, link: &Path) -> Result<()> {
    if link.symlink_metadata().is_ok() {
        if std::fs::read_link(link).ok().as_deref() == Some(target) {
            return Ok(());
        }
        if link.is_dir() && link.symlink_metadata()?.file_type().is_dir() {
            bail!("{} is a real folder (a work folder from an earlier build): move it to {}", link.display(), target.display());
        }
        std::fs::remove_file(link)?;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link)?;
    #[cfg(windows)]
    {
        let status = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).status()?;
        anyhow::ensure!(status.success(), "could not link {} to {}", link.display(), target.display());
    }
    Ok(())
}
