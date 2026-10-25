//! nxnativev2: the desktop app. The window (ui/app.slint) on the main thread; builds, sign-in, the device and installing on a tokio
//! runtime and worker threads. Everything the window shows is kept in one `State`; `render` turns it into the window's properties, so
//! each card shows its own activity (progress, result, log) whichever page is open.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use futures::FutureExt;
use nxcore::{apple, device, export, games, lc_usb, metal, nsp::Kind, pipeline, settings::Settings, store::Store, xip};
use slint::{ComponentHandle, Image, ModelRc, SharedString, VecModel, Weak};
use tokio::sync::oneshot;

slint::include_modules!();

const LOG_KEEP: usize = 400;

/// Something running or done, with its own log: a game's install, its copy into LiveContainer, LiveContainer's install, the device page's work
#[derive(Default, Clone)]
struct Activity {
    busy: bool,
    fraction: f32,
    /// the chip while busy or after a failure ("42%", "Failed")
    state: String,
    progress: String,
    failed: bool,
    log: Vec<String>,
}

impl Activity {
    fn start(&mut self, first: impl Into<String>) {
        *self = Activity { busy: true, log: std::mem::take(&mut self.log), ..Default::default() };
        self.say(first);
    }
    fn say(&mut self, line: impl Into<String>) {
        let line = line.into();
        if self.log.last() != Some(&line) {
            self.log.push(line);
            if self.log.len() > LOG_KEEP {
                self.log.drain(..self.log.len() - LOG_KEEP);
            }
        }
    }
    fn end(&mut self, result: Result<String, String>) {
        self.busy = false;
        self.fraction = 0.0;
        self.progress.clear();
        match result {
            Ok(m) => {
                self.failed = false;
                self.state.clear();
                self.say(m);
            }
            Err(e) => {
                self.failed = true;
                self.state = "Failed".into();
                self.say(e);
            }
        }
    }
}

#[derive(Default, Clone)]
struct BuildView {
    active: bool,
    failed: bool,
    label: String,
    progress: String,
    fraction: f32,
    indeterminate: bool,
    stage: i32,
    stages: i32,
    log: Vec<String>,
}

#[derive(Default)]
struct State {
    page: i32,
    selected: i32,
    games: Vec<games::Game>,
    facts: HashMap<u64, games::BuildFacts>,
    builds: HashMap<u64, BuildView>,
    running: Option<(u64, pipeline::Canceller)>,
    standalone: HashMap<u64, Activity>,
    copies: HashMap<u64, Activity>,
    lc: Activity,
    dev: Activity,
    devices: Vec<apple::Device>,
    device_index: usize,
    device: Option<device::DeviceInfo>,
    /// LiveContainer on the device: its install and the apps in it (bundle ID, copied at)
    lc_app: Option<lc_usb::LiveContainer>,
    lc_contents: Vec<(String, i64)>,
    account: Option<apple::AccountInfo>,
    signing_in: bool,
    app_ids: Vec<apple::AppIdInfo>,
    app_ids_loading: bool,
    /// the Wi-Fi share: the server, the game it serves (None: the certificate only), its address and QR code
    share: Option<(nxcore::serve::Server, Option<u64>, String, Option<(u32, Vec<u8>)>)>,
    sdk: Option<String>,
    metal: Option<String>,
    metal_version: Option<String>,
    tools_busy: bool,
    tools_label: String,
    tools_fraction: f32,
    scripts: String,
    /// updates: the latest release's status, the active bundle's version, the check (settings row) and the install (the card) under way
    update: Option<nxcore::update::Status>,
    bundle_version: Option<u64>,
    update_busy: bool,
    update_label: String,
    update_fraction: f32,
    /// the card in the corner: the release it was closed for (Later), an install under way, what it installed, and the executable to start again
    update_dismissed: String,
    update_installing: bool,
    update_step: String,
    update_done: String,
    restart_exe: Option<PathBuf>,
    /// the What's new dialog: the releases it shows, and whether they are the installed update's (closing it forgets them, update::whats_new)
    whats_new: Vec<nxcore::update::Changes>,
    whats_new_open: bool,
    whats_new_installed: bool,
    toast: String,
    toast_gen: u64,
}

struct App {
    ui: Weak<AppWindow>,
    rt: tokio::runtime::Handle,
    settings: Mutex<Settings>,
    store: Store,
    st: Mutex<State>,
    session: tokio::sync::Mutex<Option<apple::Session>>,
    tfa: Mutex<Option<oneshot::Sender<apple::TwoFactorAnswer>>>,
    certs: Mutex<Option<oneshot::Sender<Vec<String>>>>,
    dropped: Mutex<Vec<PathBuf>>,
    lc_copy: Mutex<Option<Arc<AtomicBool>>>,
    device_refreshing: AtomicBool,
    render_queued: AtomicBool,
    device_gen: AtomicU64,
}

/// Runs `f` with the window on the UI thread (from any thread)
fn on_ui(ui: &Weak<AppWindow>, f: impl FnOnce(&AppWindow) + Send + 'static) {
    let ui = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = ui.upgrade() {
            f(&w);
        }
    });
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(v)))
}

fn size(bytes: u64) -> String {
    if bytes >= 1 << 30 { format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64) } else { format!("{:.0} MB", bytes as f64 / (1u64 << 20) as f64) }
}

fn minutes(secs: f64) -> String {
    let m = (secs / 60.0).ceil() as u64;
    if m <= 1 { "a minute left".into() } else { format!("{m} min left") }
}

/// "today 14:58", "yesterday", "12 Oct"
fn when(unix: i64) -> String {
    if unix <= 0 {
        return String::new();
    }
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let days = (now - unix) / 86400;
    let tz = local_offset();
    let t = unix + tz;
    let (h, m) = ((t % 86400) / 3600, (t % 3600) / 60);
    match days {
        0 => format!("today {h:02}:{m:02}"),
        1 => "yesterday".into(),
        d if d < 7 => format!("{d} days ago"),
        _ => {
            let (y, mo, d) = civil(t / 86400);
            let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
            let _ = y;
            format!("{d} {}", months[(mo - 1) as usize])
        }
    }
}

/// The local time zone's offset in seconds (from `date +%z` on Unix; 0 when unknown)
fn local_offset() -> i64 {
    thread_local! { static OFF: i64 = {
        std::process::Command::new("date").arg("+%z").output().ok().and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let sign = if s.starts_with('-') { -1 } else { 1 };
            let d = s.trim_start_matches(['+', '-']);
            if d.len() != 4 { return None; }
            Some(sign * (d[..2].parse::<i64>().ok()? * 3600 + d[2..].parse::<i64>().ok()? * 60))
        }).unwrap_or(0)
    }; }
    OFF.with(|o| *o)
}

/// Days since 1970-01-01 → (year, month, day)
fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

fn mtime(p: &Path) -> i64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn initials(email: &str) -> String {
    email.chars().filter(|c| c.is_alphanumeric()).take(2).collect::<String>().to_uppercase()
}

thread_local! {
    /// decoded game icons (UI thread)
    static ICONS: RefCell<HashMap<PathBuf, Image>> = RefCell::new(HashMap::new());
}

fn icon(p: &Option<PathBuf>) -> (Image, bool) {
    match p {
        Some(p) => ICONS.with(|c| {
            let mut c = c.borrow_mut();
            let img = c.entry(p.clone()).or_insert_with(|| Image::load_from_path(p).unwrap_or_default()).clone();
            let ok = img.size().width > 0;
            (img, ok)
        }),
        None => (Image::default(), false),
    }
}

impl App {
    fn toast(self: &Arc<Self>, msg: impl Into<String>) {
        let generation = {
            let mut st = self.st.lock().unwrap();
            st.toast = msg.into();
            st.toast_gen += 1;
            st.toast_gen
        };
        self.render();
        let app = self.clone();
        on_ui(&self.ui, move |_| {
            slint::Timer::single_shot(Duration::from_secs(6), move || {
                let clear = {
                    let mut st = app.st.lock().unwrap();
                    let c = st.toast_gen == generation;
                    if c {
                        st.toast.clear();
                    }
                    c
                };
                if clear {
                    app.render();
                }
            });
        });
    }

    fn game(&self, id: u64) -> Option<games::Game> {
        self.st.lock().unwrap().games.iter().find(|g| g.id == id).cloned()
    }

    /// The bundle ID of a game's (or an added app's) IPA
    fn bundle_of(&self, id: u64) -> String {
        self.game(id).map(|g| g.bundle_id()).unwrap_or_else(|| games::bundle_id(id))
    }

    fn selected_id(&self) -> Option<u64> {
        let st = self.st.lock().unwrap();
        st.games.get(st.selected.max(0) as usize).map(|g| g.id)
    }

    /// The game files and keys added from a game's page go to that game (keys need one); elsewhere they find their game by title ID
    fn import_target(&self) -> Option<u64> {
        let on_game = self.st.lock().unwrap().page == 0;   // the lock ends here: selected_id takes it again
        // keys never go to an app
        if on_game { self.selected_id().filter(|id| self.game(*id).is_some_and(|g| g.app.is_none())) } else { None }
    }

    fn udid(&self) -> Option<String> {
        let st = self.st.lock().unwrap();
        st.devices.get(st.device_index).map(|d| d.udid.clone())
    }

    /// Redraws the window from the state (coalesced: many updates in a row make one redraw)
    fn render(self: &Arc<Self>) {
        if self.render_queued.swap(true, Ordering::SeqCst) {
            return;
        }
        let app = self.clone();
        on_ui(&self.ui, move |w| {
            app.render_queued.store(false, Ordering::SeqCst);
            app.draw(w);
        });
    }

    fn draw(self: &Arc<Self>, w: &AppWindow) {
        let st = self.st.lock().unwrap();
        let installed_ids: Vec<String> = st.device.as_ref().map(|d| d.apps.iter().map(|a| a.bundle_id.clone()).collect()).unwrap_or_default();
        let standalone_of = |g: &games::Game| installed_ids.iter().find(|b| b.starts_with(&g.bundle_id())).cloned();
        let copied_of = |g: &games::Game| st.lc_contents.iter().find(|(b, _)| *b == g.bundle_id()).map(|(_, t)| *t);

        w.set_page(st.page);
        w.set_selected(st.selected);
        w.set_toast(st.toast.clone().into());

        // ---- sidebar ----
        let items: Vec<GameItem> = st
            .games
            .iter()
            .map(|g| {
                let (img, has) = icon(&st.facts.get(&g.id).and_then(|f| f.icon.clone()));
                let b = st.builds.get(&g.id);
                let sa = st.standalone.get(&g.id);
                let cp = st.copies.get(&g.id);
                let (state, detail, fraction) = if let Some(b) = b.filter(|b| b.active) {
                    (1, format!("Building · {}", if b.indeterminate { b.label.clone() } else { format!("{:.0}%", b.fraction * 100.0) }), b.fraction)
                } else if let Some(a) = sa.filter(|a| a.busy) {
                    (1, format!("Installing · {:.0}%", a.fraction * 100.0), a.fraction)
                } else if let Some(a) = cp.filter(|a| a.busy) {
                    (1, format!("Copying · {:.0}%", a.fraction * 100.0), a.fraction)
                } else if !g.missing().is_empty() {
                    (2, format!("Needs {}", g.missing().join(" and ")), 0.0)
                } else {
                    let places: Vec<&str> = [standalone_of(g).map(|_| "Standalone"), copied_of(g).map(|_| "in LiveContainer")].into_iter().flatten().collect();
                    if !places.is_empty() {
                        (3, places.join(" · "), 0.0)
                    } else if g.ipa.is_some() {
                        (0, "Ready to install".into(), 0.0)
                    } else if b.is_some_and(|b| b.failed) {
                        (2, "Build failed".into(), 0.0)
                    } else {
                        (0, "Ready to build".into(), 0.0)
                    }
                };
                GameItem { id: format!("{:016X}", g.id).into(), name: g.name.clone().into(), icon: img, has_icon: has, state, detail: detail.into(), fraction }
            })
            .collect();
        w.set_games(model(items));
        let lc = st.lc_app.as_ref();
        w.set_lc_state(if st.lc.busy { 1 } else if lc.is_some() { 3 } else { 2 });
        w.set_lc_detail(
            match lc {
                Some(l) => format!("{} · {} inside", l.version, st.lc_contents.len()),
                None if st.device.is_some() => "Not installed".into(),
                None => String::new(),
            }
            .into(),
        );
        let dev = st.device.as_ref();
        w.set_device_connected(dev.is_some());
        if let Some(d) = dev {
            w.set_device_name(device::short_name(&d.model).into());
            let os = if d.product_type.starts_with("iPad") { "iPadOS" } else { "iOS" };
            w.set_device_detail(format!("{} · {os} {}", if st.devices.get(st.device_index).is_some_and(|x| !x.usb) { "Wi-Fi" } else { "USB" }, d.ios).into());
        }
        let email = st.account.as_ref().map(|a| a.email.clone()).unwrap_or_default();
        w.set_signed_in_as(email.clone().into());
        w.set_initials(initials(&email).into());
        w.set_signing_in(st.signing_in);

        // ---- the selected game ----
        if let Some(g) = st.games.get(st.selected.max(0) as usize) {
            let facts = st.facts.get(&g.id).cloned().unwrap_or_default();
            let (img, has) = icon(&facts.icon);
            w.set_g_title(g.name.clone().into());
            w.set_g_icon(img);
            w.set_g_has_icon(has);
            let ipa_size = g.ipa.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len());
            let facts_line: Vec<String> = [
                facts.display_version.as_ref().map(|v| format!("Version {v}")),
                ipa_size.map(size),
                facts.shaders.map(|n| format!("{} shaders", thousands(n as u64))),
                (!g.supported).then(|| "no profile".to_string()),
                g.app.as_ref().map(|a| a.bundle_id.clone()),
            ]
            .into_iter()
            .flatten()
            .collect();
            w.set_g_facts(facts_line.join("  ·  ").into());
            let mut chips = vec![];
            if g.app.is_some() {
                chips.push(ChipData { label: "iOS app".into(), kind: 3 });
            } else {
                chips.push(if g.has_base() { ChipData { label: "Base game".into(), kind: 0 } } else { ChipData { label: "Base game missing".into(), kind: 1 } });
                for f in g.files.iter().filter(|f| f.kind == Kind::Update) {
                    let v = facts.display_version.clone().filter(|_| Some(f.version) == Some(g.newest_update())).unwrap_or_else(|| f.version.map_or("?".into(), |v| format!("v{v}")));
                    chips.push(ChipData { label: format!("Update {v}").into(), kind: 0 });
                }
                if g.dlc_count() > 0 {
                    chips.push(ChipData { label: format!("{} DLC", g.dlc_count()).into(), kind: 0 });
                }
                chips.push(if g.has_keys { ChipData { label: "Keys".into(), kind: 0 } } else { ChipData { label: "prod.keys missing".into(), kind: 1 } });
                chips.push(ChipData { label: "+ Add".into(), kind: 2 });
            }
            w.set_g_chips(model(chips));
            w.set_g_app(g.app.is_some());
            w.set_g_ready(g.missing().is_empty());
            w.set_g_device_shaders(self.settings.lock().unwrap().shaders_on_device(g.id));
            w.set_g_built(g.ipa.is_some());
            let made = if g.app.is_some() { "Added" } else { "Built" };
            w.set_g_built_line(g.ipa.as_ref().map(|p| format!("{made} {}", when(mtime(p)))).unwrap_or_default().into());

            let b = st.builds.get(&g.id).cloned().unwrap_or_default();
            w.set_b_active(b.active);
            w.set_b_failed(b.failed);
            w.set_b_label(b.label.into());
            w.set_b_progress(b.progress.into());
            w.set_b_fraction(b.fraction);
            w.set_b_indeterminate(b.indeterminate);
            w.set_b_stage(b.stage);
            w.set_b_stages(b.stages);
            w.set_b_log(lines(&b.log));

            let sa = st.standalone.get(&g.id).cloned().unwrap_or_default();
            let installed = standalone_of(g);
            w.set_sa_installed(installed.is_some());
            w.set_sa_busy(sa.busy);
            w.set_sa_fraction(sa.fraction);
            w.set_sa_progress(sa.progress.clone().into());
            w.set_sa_log(lines(&sa.log));
            let (state, kind) = if sa.busy {
                (if sa.fraction > 0.0 { format!("{:.0}%", sa.fraction * 100.0) } else { "Starting".into() }, 3)
            } else if sa.failed {
                ("Failed".into(), 1)
            } else if installed.is_some() {
                ("Installed".into(), 0)
            } else if dev.is_some() {
                ("Not installed".into(), 2)
            } else {
                (String::new(), 2)
            };
            w.set_sa_state(state.into());
            w.set_sa_kind(kind);
            w.set_sa_line(SharedString::new());

            let cp = st.copies.get(&g.id).cloned().unwrap_or_default();
            let copied = copied_of(g);
            w.set_lcd_copied(copied.is_some());
            w.set_lcd_busy(cp.busy);
            w.set_lcd_fraction(cp.fraction);
            w.set_lcd_progress(cp.progress.clone().into());
            w.set_lcd_log(lines(&cp.log));
            let (state, kind) = if cp.busy {
                (if cp.fraction > 0.0 { format!("{:.0}%", cp.fraction * 100.0) } else { "Starting".into() }, 3)
            } else if cp.failed {
                ("Failed".into(), 1)
            } else if copied.is_some() {
                ("Copied".into(), 0)
            } else if lc.is_some() {
                ("Not copied".into(), 2)
            } else {
                (String::new(), 2)
            };
            w.set_lcd_state(state.into());
            w.set_lcd_kind(kind);
            w.set_lcd_line(
                match (cp.busy, copied) {
                    (false, Some(t)) => when(t),
                    _ => String::new(),
                }
                .into(),
            );
            let (active, game) = match &st.share {
                Some((_, game, _, _)) => (true, *game == Some(g.id)),
                None => (false, false),
            };
            w.set_share_active(active);
            w.set_share_game(game);
        }
        if let Some((_, game, url, qr)) = &st.share {
            if st.page == 1 || (st.page == 0 && *game == st.games.get(st.selected.max(0) as usize).map(|g| g.id)) {
                w.set_share_url(url.clone().into());
                if let Some((n, px)) = qr {
                    w.set_share_qr(Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(px, *n, *n)));
                }
            }
            if st.page == 1 {
                w.set_share_game(game.is_some());
            }
        } else {
            w.set_share_active(false);
        }

        // ---- LiveContainer ----
        let lc_more = lc.and_then(|l| st.app_ids.iter().find(|a| a.identifier == l.bundle_id)).and_then(|a| a.memory_limit);
        w.set_lc_installed(lc.is_some());
        w.set_lc_standard(lc_more == Some(false));
        w.set_lc_facts(lc.map(|l| format!("Version {}", l.version)).unwrap_or_default().into());
        let mut lc_chips = vec![];
        if let Some(_) = lc {
            lc_chips.push(ChipData { label: "Installed".into(), kind: 0 });
            match lc_more {
                Some(true) => lc_chips.push(ChipData { label: "More memory".into(), kind: 0 }),
                Some(false) => lc_chips.push(ChipData { label: "Standard memory".into(), kind: 1 }),
                None => {}
            }
        } else if dev.is_some() {
            lc_chips.push(ChipData { label: "Not installed".into(), kind: 1 });
        }
        w.set_lc_chips(model(lc_chips));
        w.set_lc_busy(st.lc.busy);
        w.set_lc_fraction(st.lc.fraction);
        w.set_lc_progress(st.lc.progress.clone().into());
        w.set_lc_log(lines(&st.lc.log));
        w.set_lc_state_label(
            if st.lc.busy {
                if st.lc.fraction > 0.0 { format!("{:.0}%", st.lc.fraction * 100.0) } else { "Starting".into() }
            } else if st.lc.failed {
                "Failed".into()
            } else if lc.is_some() {
                "Installed".into()
            } else {
                String::new()
            }
            .into(),
        );
        let mut lc_games: Vec<LcGame> = vec![];
        for (bundle, t) in &st.lc_contents {
            let idx = st.games.iter().position(|g| g.bundle_id() == *bundle);
            let g = idx.map(|i| &st.games[i]);
            let (img, has) = icon(&g.and_then(|g| st.facts.get(&g.id)).and_then(|f| f.icon.clone()));
            let newer = g.and_then(|g| g.ipa.as_ref()).is_some_and(|p| mtime(p) > *t);
            lc_games.push(LcGame {
                name: g.map(|g| g.name.clone()).unwrap_or_else(|| bundle.clone()).into(),
                icon: img,
                has_icon: has,
                detail: format!("Copied {}", when(*t)).into(),
                action: if g.is_some_and(|g| g.ipa.is_some()) { "Update".into() } else { "".into() },
                newer,
                game: idx.map(|i| i as i32).unwrap_or(-1),
            });
        }
        for (i, g) in st.games.iter().enumerate().filter(|(_, g)| g.ipa.is_some() && copied_of(g).is_none()) {
            let (img, has) = icon(&st.facts.get(&g.id).and_then(|f| f.icon.clone()));
            lc_games.push(LcGame { name: g.name.clone().into(), icon: img, has_icon: has, detail: SharedString::new(), action: "Copy".into(), newer: true, game: i as i32 });
        }
        w.set_lc_games(model(lc_games));

        // ---- the device and the Apple ID ----
        w.set_devices(model(st.devices.iter().map(|d| SharedString::from(d.name.clone())).collect()));
        w.set_device_index(st.device_index as i32);
        if let Some(d) = dev {
            w.set_d_title(device::short_name(&d.model).into());
            let os = if d.product_type.starts_with("iPad") { "iPadOS" } else { "iOS" };
            w.set_d_line(format!("{}  ·  {os} {}", d.model, d.ios).into());
            let mut chips = vec![];
            match d.developer_mode {
                Some(true) => chips.push(ChipData { label: "Developer Mode".into(), kind: 0 }),
                Some(false) => chips.push(ChipData { label: "Developer Mode off".into(), kind: 1 }),
                None => {}
            }
            w.set_d_chips(model(chips));
            let (cable, state) = match d.link_mbps {
                Some(m) if m <= 480 => (format!("USB 2  ·  {m} Mbit/s"), 1),
                Some(m) => (format!("USB 3  ·  {} Gbit/s", m / 1000), 0),
                None if st.devices.get(st.device_index).is_some_and(|x| !x.usb) => ("Wi-Fi".into(), 1),
                None => ("USB".into(), 3),
            };
            w.set_d_cable(cable.into());
            w.set_d_cable_state(state);
            w.set_d_space(match (d.free, d.total) {
                (Some(f), Some(t)) => format!("{} free of {}", size(f), size(t)),
                _ => "—".into(),
            }
            .into());
        } else {
            w.set_d_chips(model(vec![]));
        }
        w.set_d_busy(st.dev.busy);
        w.set_d_fraction(st.dev.fraction);
        w.set_d_label(if st.dev.busy || st.dev.failed { st.dev.log.last().cloned().unwrap_or_default() } else { st.dev.log.last().cloned().unwrap_or_default() }.into());
        w.set_d_log(lines(&st.dev.log));
        if let Some(a) = &st.account {
            w.set_a_team(format!("Team {}", a.team_id).into());
            let (cert, state) = match a.certificate_expires.and_then(|t| t.duration_since(SystemTime::now()).ok()) {
                Some(left) if left.as_secs() < 86400 => ("Expires today".to_string(), 1),
                Some(left) => (format!("Valid {} more days", left.as_secs() / 86400), 0),
                None => ("None yet".to_string(), 3),
            };
            w.set_a_cert(cert.into());
            w.set_a_cert_state(state);
        }
        w.set_app_ids(model(
            st.app_ids
                .iter()
                .map(|a| AppIdRow { name: a.name.clone().into(), identifier: a.identifier.clone().into(), more: a.memory_limit == Some(true) })
                .collect(),
        ));
        w.set_app_ids_loading(st.app_ids_loading);

        // ---- settings ----
        w.set_s_sdk_ok(st.sdk.is_some());
        w.set_s_sdk(st.sdk.clone().unwrap_or_else(|| "Xcode .xip".into()).into());
        w.set_s_metal_ok(st.metal.is_some());
        w.set_s_metal(st.metal.clone().unwrap_or_else(|| "Metal Developer Tools".into()).into());
        w.set_s_tools_busy(st.tools_busy);
        w.set_s_tools_label(st.tools_label.clone().into());
        w.set_s_tools_fraction(st.tools_fraction);
        w.set_s_data(self.store.root().display().to_string().into());
        w.set_s_scripts(st.scripts.clone().into());
        w.set_u_app(nxcore::update::client_version().into());
        w.set_u_bundle(st.bundle_version.map_or("None yet".into(), |v| v.to_string()).into());
        w.set_u_label(st.update_label.clone().into());
        w.set_u_busy(st.update_busy);
        w.set_u_fraction(st.update_fraction);
        let available = st.update.as_ref().is_some_and(|u| u.client.is_some() || (u.bundle.is_some() && !u.bundle_needs_client));
        w.set_u_available(available);
        let offer = available && st.update.as_ref().is_some_and(|u| u.manifest.tag != st.update_dismissed);
        w.set_u_card(offer || st.update_installing || !st.update_done.is_empty());
        w.set_u_card_text(
            if st.update_installing { st.update_step.clone() } else if !st.update_done.is_empty() { st.update_done.clone() } else { st.update.as_ref().map(Self::describe).unwrap_or_default() }.into(),
        );
        w.set_u_installing(st.update_installing);
        w.set_u_restart(st.restart_exe.is_some());
        w.set_u_finished(!st.update_done.is_empty());
        // the card's button: the checked release's changes newer than what this computer has; the open dialog: what it was opened with
        let shown = if st.whats_new_open { st.whats_new.clone() } else if offer { st.update.as_ref().map(|u| nxcore::update::changes_since(&u.manifest, Self::installed(st.bundle_version))).unwrap_or_default() } else { vec![] };
        w.set_whats_new(model(
            shown.iter().map(|c| ReleaseChanges { title: format!("{} · {}", c.tag, c.published.get(..10).unwrap_or(&c.published)).into(), notes: model(c.notes.iter().map(|n| SharedString::from(n.as_str())).collect()) }).collect(),
        ));
        w.set_whats_new_open(st.whats_new_open);
        w.set_whats_new_title(if st.whats_new_installed { "What's new in this update" } else { "What's new" }.into());
    }

    /// The newest release this computer has (update::installed_release, from the bundle version the state holds)
    fn installed(bundle: Option<u64>) -> u64 {
        [nxcore::update::release_number(), bundle.unwrap_or(0)].into_iter().filter(|&n| n > 0).min().unwrap_or(0)
    }

    /// The card's What's new: the checked release's changes newer than this computer's
    fn show_whats_new(self: &Arc<Self>) {
        {
            let mut st = self.st.lock().unwrap();
            let since = Self::installed(st.bundle_version);
            let Some(u) = &st.update else { return };
            st.whats_new = nxcore::update::changes_since(&u.manifest, since);
            st.whats_new_installed = false;
            st.whats_new_open = !st.whats_new.is_empty();
        }
        self.render();
    }

    /// OK: an installed update's changes are not shown again
    fn close_whats_new(self: &Arc<Self>) {
        let installed = {
            let mut st = self.st.lock().unwrap();
            st.whats_new_open = false;
            std::mem::take(&mut st.whats_new_installed)
        };
        if installed {
            nxcore::update::clear_whats_new(&self.store);
        }
        self.render();
    }

    // ---------------------------------------------------------------- games

    fn refresh_games(self: &Arc<Self>, select: Option<u64>) {
        let repo = pipeline::repo(&self.settings.lock().unwrap(), &self.store);
        let list = games::list(&self.store, repo.as_deref()).unwrap_or_default();
        let facts: HashMap<u64, games::BuildFacts> = list.iter().map(|g| (g.id, games::build_facts(&self.store, g.id))).collect();
        {
            let mut st = self.st.lock().unwrap();
            let keep = select.or_else(|| st.games.get(st.selected.max(0) as usize).map(|g| g.id));
            st.selected = keep.and_then(|id| list.iter().position(|g| g.id == id)).unwrap_or(0) as i32;
            st.games = list;
            st.facts = facts;
            if select.is_some() {
                st.page = 0;
            }
        }
        self.render();
    }

    fn import(self: &Arc<Self>, paths: Vec<PathBuf>, target: Option<u64>) {
        let app = self.clone();
        std::thread::spawn(move || {
            app.toast("Adding…");
            let progress = |d: u64, t: u64| {
                if t > 0 && d * 10 / t != d.saturating_sub(1 << 26) * 10 / t {
                    app.toast(format!("Adding… {}%", d * 100 / t));
                }
            };
            match games::import(&app.store, &paths, target, &progress) {
                Ok(r) => {
                    let mut msg = format!("Added {} file{}", r.imported.len(), if r.imported.len() == 1 { "" } else { "s" });
                    if let Some((p, why)) = r.skipped.first() {
                        msg.push_str(&format!(" · skipped {}: {why}", p.file_name().unwrap_or_default().to_string_lossy()));
                    }
                    app.toast(msg);
                    app.refresh_games(r.games.first().copied());
                }
                Err(e) => app.toast(format!("Could not add: {e:#}")),
            }
        });
    }

    fn update_build(self: &Arc<Self>, id: u64, f: impl FnOnce(&mut BuildView)) {
        f(self.st.lock().unwrap().builds.entry(id).or_default());
        self.render();
    }

    /// Unpacks the app's build tools when needed, then builds
    fn build(self: &Arc<Self>, id: u64) {
        if self.game(id).is_some_and(|g| g.app.is_some()) {
            return;
        }
        if self.st.lock().unwrap().running.is_some() {
            self.toast("Another game is building");
            return;
        }
        let settings = self.settings.lock().unwrap().clone();
        let missing: Vec<pipeline::Requirement> = pipeline::check(&settings, &self.store).into_iter().filter(|r| !r.ok).collect();
        if !missing.is_empty() {
            let apple = missing.iter().any(|r| r.name.contains("SDK") || r.name.contains("Metal"));
            if apple {
                self.st.lock().unwrap().page = 3;
            }
            self.toast(format!("Missing: {}", missing.iter().map(|r| format!("{} ({})", r.name, r.detail)).collect::<Vec<_>>().join("; ")));
            return;
        }
        let app = self.clone();
        self.update_build(id, |v| {
            *v = BuildView { active: true, label: "Preparing".into(), indeterminate: true, stage: -1, log: std::mem::take(&mut v.log), ..Default::default() };
            v.log.push("— build —".into());
        });
        self.rt.spawn(async move {
            let a2 = app.clone();
            let progress = move |d: u64, t: u64, step: &str| {
                let step = step.to_string();
                a2.update_build(id, |v| {
                    v.label = step;
                    v.indeterminate = t == 0;
                    v.fraction = if t > 0 { d as f32 / t as f32 } else { 0.0 };
                });
            };
            match pipeline::prepare(&settings, &app.store, &progress).await {
                Ok(()) => app.start_build(id),
                Err(e) => app.update_build(id, |v| {
                    v.active = false;
                    v.failed = true;
                    v.label = "Preparing failed".into();
                    v.log.push(format!("{e:#}"));
                }),
            }
        });
    }

    fn start_build(self: &Arc<Self>, id: u64) {
        let settings = self.settings.lock().unwrap().clone();
        let app = self.clone();
        let events = {
            let app = app.clone();
            move |e: pipeline::Event| match e {
                pipeline::Event::Stage { k, n, name } => app.update_build(id, |v| {
                    v.label = capitalize(&name);
                    v.stage = k as i32 - 1;
                    v.stages = n as i32;
                    v.fraction = 0.0;
                    v.indeterminate = true;
                    v.progress.clear();
                }),
                pipeline::Event::Progress { step, frac, count, eta, .. } => app.update_build(id, |v| {
                    v.indeterminate = frac.is_none();
                    v.fraction = frac.unwrap_or(0.0) as f32;
                    // the step and its count together: a stage's steps can each count to the same total (iOS shaders: "cache check (2 of 6)", "translating (3 of 6)", ...)
                    let step = step.chars().next().map(|c| c.to_uppercase().collect::<String>() + &step[c.len_utf8()..]).unwrap_or_default();
                    v.progress = [step, count, eta.map(minutes).unwrap_or_default()]
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("  ·  ");
                }),
                pipeline::Event::StageEnd { name, ok, summary } => app.update_build(id, |v| {
                    v.log.push(format!("{} {name}{}", if ok { "✓" } else { "✗" }, if summary.is_empty() { String::new() } else { format!(": {summary}") }));
                }),
                pipeline::Event::Say(m) | pipeline::Event::Log(m) => app.update_build(id, |v| {
                    v.log.push(m);
                    if v.log.len() > LOG_KEEP {
                        v.log.drain(..v.log.len() - LOG_KEEP);
                    }
                }),
            }
        };
        let opts = pipeline::Options { show_fps: settings.show_fps, shaders_on_device: settings.shaders_on_device(id), debug: settings.debug_builds, ..Default::default() };
        match pipeline::start(&settings, &self.store, id, &opts, events) {
            Ok(handle) => {
                self.st.lock().unwrap().running = Some((id, handle.canceller()));
                std::thread::spawn(move || {
                    let result = handle.wait();
                    let cancelled = app.st.lock().unwrap().running.take().is_none();
                    app.update_build(id, |v| {
                        v.active = false;
                        match &result {
                            Ok(()) => {
                                v.failed = false;
                                v.label = "Done".into();
                                v.log.push("✓ the IPA is ready".into());
                            }
                            Err(e) => {
                                v.failed = !cancelled;
                                v.label = if cancelled { "Cancelled".into() } else { "Failed".into() };
                                v.log.push(e.to_string());
                            }
                        }
                    });
                    if result.is_ok() {
                        app.toast(format!("{} is built", app.game(id).map(|g| g.name).unwrap_or_default()));
                    }
                    app.refresh_games(None);
                });
            }
            Err(e) => self.update_build(id, |v| {
                v.active = false;
                v.failed = true;
                v.label = "Could not start".into();
                v.log.push(format!("{e:#}"));
            }),
        }
        self.render();
    }

    fn cancel_build(self: &Arc<Self>) {
        let c = self.st.lock().unwrap().running.take();
        if let Some((id, c)) = c {
            c.cancel();
            self.update_build(id, |v| v.log.push("cancelled".into()));
        }
    }

    // ---------------------------------------------------------------- the device

    /// Lists the devices; when they changed (or `force`), reads the selected one's details and LiveContainer
    fn refresh_device(self: &Arc<Self>, force: bool) {
        if self.device_refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let app = self.clone();
        self.rt.spawn(async move {
            let devs = apple::devices().await.unwrap_or_default();
            let (changed, udid) = {
                let mut st = app.st.lock().unwrap();
                let before: Vec<String> = st.devices.iter().map(|d| d.udid.clone()).collect();
                let now: Vec<String> = devs.iter().map(|d| d.udid.clone()).collect();
                let changed = before != now;
                st.devices = devs;
                if st.device_index >= st.devices.len() {
                    st.device_index = 0;
                }
                (changed, st.devices.get(st.device_index).map(|d| d.udid.clone()))
            };
            if changed || force {
                let generation = app.device_gen.fetch_add(1, Ordering::SeqCst) + 1;
                let (info, lc, contents) = match &udid {
                    Some(u) => {
                        let info = device::info(u).await.map_err(|e| tracing::warn!("device info: {e:#}")).ok();
                        let lc = lc_usb::find(u).await.ok().and_then(|l| l.into_iter().next());
                        let contents = match &lc {
                            Some(l) => lc_usb::contents(u, &l.bundle_id).await.unwrap_or_default(),
                            None => vec![],
                        };
                        (info, lc, contents)
                    }
                    None => (None, None, vec![]),
                };
                if app.device_gen.load(Ordering::SeqCst) == generation {
                    let mut st = app.st.lock().unwrap();
                    st.device = info;
                    st.lc_app = lc;
                    st.lc_contents = contents;
                }
                app.render();
            }
            app.device_refreshing.store(false, Ordering::SeqCst);
        });
    }

    // ---------------------------------------------------------------- Apple ID

    fn sign_in(self: &Arc<Self>, email: String, password: String, remember: bool) {
        let email = email.trim().to_string();
        let saved = apple::credentials::load(&email);
        let (password, from_keyring) = match (password.is_empty(), saved) {
            (true, Some(p)) => (p, true),
            (_, _) => (password, false),
        };
        if email.is_empty() || password.is_empty() {
            return;
        }
        let app = self.clone();
        self.st.lock().unwrap().signing_in = true;
        self.render();
        let ask: apple::TwoFactorAsk = {
            let app = app.clone();
            Arc::new(move |p: apple::TwoFactorPrompt| {
                let (tx, rx) = oneshot::channel();
                *app.tfa.lock().unwrap() = Some(tx);
                let mut msg = if p.sms { "Sent by SMS.".to_string() } else { "Shown on your iPhone, iPad or Mac.".to_string() };
                if let Some(e) = &p.last_error {
                    msg = format!("{e}. {msg}");
                }
                let numbers: Vec<Choice> = p.numbers.iter().map(|(id, n)| Choice { key: id.to_string().into(), label: n.clone().into() }).collect();
                on_ui(&app.ui, move |w| {
                    w.set_tfa_message(msg.into());
                    w.set_tfa_numbers(model(numbers));
                    w.set_tfa_code("".into());
                    w.set_tfa_open(true);
                });
                async move { rx.await.unwrap_or(apple::TwoFactorAnswer::Abort) }.boxed()
            })
        };
        let certs: apple::CertsAsk = {
            let app = app.clone();
            Arc::new(move |list: Vec<(String, String)>| {
                let (tx, rx) = oneshot::channel();
                *app.certs.lock().unwrap() = Some(tx);
                let items: Vec<Choice> = list.into_iter().map(|(name, serial)| Choice { key: serial.into(), label: name.into() }).collect();
                on_ui(&app.ui, move |w| {
                    w.set_certs(model(items));
                    w.set_certs_open(true);
                });
                async move { rx.await.unwrap_or_default() }.boxed()
            })
        };
        self.rt.spawn(async move {
            let settings = app.settings.lock().unwrap().clone();
            // in its own task: a panic inside the sign-in ends as an error, not as a button stuck on "Signing in…"
            let (store2, email2, password2) = (app.store.clone(), email.clone(), password.clone());
            let result = match tokio::spawn(async move { apple::login(&store2, &settings, &email2, &password2, ask, certs).await }).await {
                Ok(r) => r,
                Err(e) => Err(anyhow::anyhow!("the sign-in stopped unexpectedly: {e}")),
            };
            if result.is_err() && from_keyring {
                apple::credentials::forget(&email);
            } else if result.is_ok() && remember && !from_keyring {
                let _ = apple::credentials::save(&email, &password);
            }
            let saved_now = apple::credentials::load(&email).is_some();
            on_ui(&app.ui, move |w| {
                w.set_has_saved_password(saved_now);
                w.set_tfa_open(false);
                w.set_password("".into());
            });
            app.st.lock().unwrap().signing_in = false;
            match result {
                Ok(mut session) => {
                    let account = session.account(&app.store).await.unwrap_or(apple::AccountInfo {
                        email: session.email().to_string(),
                        team_id: String::new(),
                        certificate_expires: None,
                    });
                    let ids = session.app_ids().await.unwrap_or_default();
                    {
                        let mut s = app.settings.lock().unwrap();
                        s.apple_id = Some(account.email.clone());
                        let _ = s.save();
                    }
                    *app.session.lock().await = Some(session);
                    {
                        let mut st = app.st.lock().unwrap();
                        st.account = Some(account);
                        st.app_ids = ids;
                    }
                    app.render();
                }
                Err(e) => {
                    tracing::error!("sign-in failed: {e:#}");
                    app.toast(format!("Sign-in failed: {e:#}"));
                }
            }
        });
    }

    fn answer_tfa(&self, a: apple::TwoFactorAnswer) {
        if let Some(tx) = self.tfa.lock().unwrap().take() {
            let _ = tx.send(a);
        }
        on_ui(&self.ui, |w| w.set_tfa_open(false));
    }

    /// Reloads the account's details (certificate, App IDs) after something changed them
    fn refresh_account(self: &Arc<Self>) {
        let app = self.clone();
        self.rt.spawn(async move {
            let mut guard = app.session.lock().await;
            let Some(s) = guard.as_mut() else { return };
            let account = s.account(&app.store).await.ok();
            let ids = s.app_ids().await.ok();
            drop(guard);
            {
                let mut st = app.st.lock().unwrap();
                if let Some(a) = account {
                    st.account = Some(a);
                }
                if let Some(ids) = ids {
                    st.app_ids = ids;
                }
                st.app_ids_loading = false;
            }
            app.render();
        });
    }

    fn need_sign_in(self: &Arc<Self>) -> bool {
        if self.st.lock().unwrap().account.is_some() {
            return false;
        }
        self.st.lock().unwrap().page = 2;
        self.toast("Sign in with your Apple ID first");
        true
    }

    // ---------------------------------------------------------------- installing

    /// Runs `work` as the activity chosen by `slot`, with its progress, result and log there
    fn activity<F, Fut>(self: &Arc<Self>, slot: fn(&mut State) -> &mut Activity, first: &str, work: F)
    where
        F: FnOnce(Arc<App>, Arc<dyn Fn(String, f32) + Send + Sync>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String>> + Send + 'static,
    {
        slot(&mut self.st.lock().unwrap()).start(first);
        self.render();
        let app = self.clone();
        let say: Arc<dyn Fn(String, f32) + Send + Sync> = {
            let app = app.clone();
            Arc::new(move |line: String, f: f32| {
                {
                    let mut st = app.st.lock().unwrap();
                    let a = slot(&mut st);
                    a.fraction = f.clamp(0.0, 0.99);
                    let head = line.split('…').next().unwrap_or(&line).to_string();
                    if a.log.last().map(|l| l.split('…').next().unwrap_or(l)) != Some(&head) {
                        a.say(line.clone());
                    } else if let Some(l) = a.log.last_mut() {
                        *l = line.clone();
                    }
                }
                app.render();
            })
        };
        self.rt.spawn(async move {
            let r = work(app.clone(), say).await;
            if let Err(e) = &r {
                tracing::error!("{e:#}");
            }
            slot(&mut app.st.lock().unwrap()).end(r.map_err(|e| format!("{e:#}")));
            app.render();
            app.refresh_device(true);
        });
    }

    fn sa_install(self: &Arc<Self>, id: u64) {
        if self.need_sign_in() {
            return;
        }
        let Some(udid) = self.udid() else { return };
        let ipa = self.store.ipa(id);
        let memory = self.settings.lock().unwrap().increased_memory_limit;
        self.game_activity(id, false, "Signing and installing", move |app, say| async move {
            let mut guard = app.session.lock().await;
            let session = guard.as_mut().ok_or_else(|| anyhow::anyhow!("not signed in"))?;
            let s2 = say.clone();
            apple::install(session, &udid, &ipa, memory, Arc::new(move |f| s2(format!("Installing… {:.0}%", f * 100.0), f))).await?;
            drop(guard);
            app.refresh_account();
            Ok(format!("Installed{}. First time: on the device, Settings › General › VPN & Device Management › trust your Apple ID", if memory { " with more RAM" } else { "" }))
        });
    }

    /// An activity of one game (its standalone install, or its copy into LiveContainer)
    fn game_activity<F, Fut>(self: &Arc<Self>, id: u64, copy: bool, first: &str, work: F)
    where
        F: FnOnce(Arc<App>, Arc<dyn Fn(String, f32) + Send + Sync>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String>> + Send + 'static,
    {
        game_slot(&mut self.st.lock().unwrap(), id, copy).start(first);
        self.render();
        let app = self.clone();
        let say: Arc<dyn Fn(String, f32) + Send + Sync> = {
            let app = app.clone();
            Arc::new(move |line: String, f: f32| {
                {
                    let mut st = app.st.lock().unwrap();
                    let a = game_slot(&mut st, id, copy);
                    a.fraction = f.clamp(0.0, 0.99);
                    let head = line.split('…').next().unwrap_or(&line).to_string();
                    if a.log.last().map(|l| l.split('…').next().unwrap_or(l).to_string()) == Some(head) {
                        *a.log.last_mut().unwrap() = line.clone();
                    } else {
                        a.say(line.clone());
                    }
                }
                app.render();
            })
        };
        self.rt.spawn(async move {
            let r = work(app.clone(), say).await;
            if let Err(e) = &r {
                tracing::error!("{e:#}");
            }
            game_slot(&mut app.st.lock().unwrap(), id, copy).end(r.map_err(|e| format!("{e:#}")));
            app.render();
            app.refresh_device(true);
        });
    }

    fn sa_remove(self: &Arc<Self>, id: u64) {
        let Some(udid) = self.udid() else { return };
        let ours = self.bundle_of(id);
        let bundle = self.st.lock().unwrap().device.as_ref().and_then(|d| d.apps.iter().find(|a| a.bundle_id.starts_with(&ours)).map(|a| a.bundle_id.clone()));
        let Some(bundle) = bundle else { return };
        self.game_activity(id, false, "Removing", move |_app, _say| async move {
            device::uninstall(&udid, &bundle).await?;
            Ok("Removed from the device".to_string())
        });
    }

    /// Copies the game into LiveContainer through the cable (LiveContainer signs it at its first launch)
    fn lcd_copy(self: &Arc<Self>, id: u64) {
        let Some(udid) = self.udid() else { return };
        let Some(lc) = self.st.lock().unwrap().lc_app.as_ref().map(|l| l.bundle_id.clone()) else { return };
        let ipa = self.store.ipa(id);
        let cancel = Arc::new(AtomicBool::new(false));
        *self.lc_copy.lock().unwrap() = Some(cancel.clone());
        self.game_activity(id, true, "Copying over USB", move |app, say| async move {
            let start = std::time::Instant::now();
            let s2 = say.clone();
            let progress = Arc::new(move |d: u64, total: u64| {
                let f = d as f32 / total.max(1) as f32;
                let rate = d as f64 / 1e6 / start.elapsed().as_secs_f64().max(0.1);
                let left = (total - d.min(total)) as f64 / 1e6 / rate.max(0.1);
                s2(format!("Copying… {} of {}  ·  {rate:.0} MB/s  ·  {}", size(d), size(total), minutes(left)), f);
            });
            let r = lc_usb::copy(&udid, &lc, &ipa, progress, cancel).await;
            *app.lc_copy.lock().unwrap() = None;
            let c = r.map_err(|e| if format!("{e:#}").contains("cancelled") { anyhow::anyhow!("Cancelled") } else { e })?;
            let what = if c.diff {
                format!("Updated {} changed file{} ({} of {})", c.sent_files, if c.sent_files == 1 { "" } else { "s" }, size(c.sent_bytes), size(c.total_bytes))
            } else {
                format!("Copied {}", size(c.sent_bytes))
            };
            Ok(format!(
                "{what} in {:.0} s{}. Reopen LiveContainer: its first launch signs the game",
                start.elapsed().as_secs_f64(),
                if c.replaced { ", saves kept" } else { "" }
            ))
        });
    }

    fn cancel_copy(&self) {
        if let Some(c) = self.lc_copy.lock().unwrap().as_ref() {
            c.store(true, Ordering::Relaxed);
        }
    }

    /// The Wi-Fi share: a game's IPA and the certificate (`id`), or the certificate alone (None)
    fn share(self: &Arc<Self>, id: Option<u64>) {
        let ipa = id.map(|id| self.store.ipa(id));
        let name = id.and_then(|id| self.game(id)).map(|g| g.name).unwrap_or_else(|| "Certificate for LiveContainer".into());
        let app = self.clone();
        self.stop_share();
        self.rt.spawn(async move {
            let certificate = {
                let mut guard = app.session.lock().await;
                match guard.as_mut() {
                    Some(s) => s.existing_certificate_p12(&app.store, "").await.ok().map(|(p, _)| p),
                    None => None,
                }
            };
            if certificate.is_none() && ipa.is_none() {
                app.toast("No certificate yet: install something with your Apple ID once");
                return;
            }
            match nxcore::serve::start(nxcore::serve::Share { name, ipa, certificate }, nxcore::serve::DEFAULT_PORT) {
                Ok(server) => {
                    let url = server.url.clone();
                    let qr = nxcore::serve::qr_rgba(&url, 6);
                    app.st.lock().unwrap().share = Some((server, id, url, qr));
                    app.render();
                }
                Err(e) => app.toast(format!("Cannot share over Wi-Fi: {e:#}")),
            }
        });
    }

    fn stop_share(self: &Arc<Self>) {
        self.st.lock().unwrap().share = None;
        self.render();
    }

    /// The game's log and the newest crash reports from the device into <data>/diag/<id>; opens the folder and shows the log's last red line (or its last line)
    fn device_log(self: &Arc<Self>, id: u64) {
        let Some(udid) = self.udid() else { return };
        let bundle = self.bundle_of(id);
        let (lc, standalone) = {
            let st = self.st.lock().unwrap();
            (
                st.lc_app.as_ref().map(|l| l.bundle_id.clone()),
                st.device.as_ref().and_then(|d| d.apps.iter().find(|a| a.bundle_id.starts_with(&bundle)).map(|a| a.bundle_id.clone())),
            )
        };
        let out = self.store.root().join("diag").join(format!("{id:016X}"));
        let app = self.clone();
        self.toast("Reading the log from the device…");
        self.rt.spawn(async move {
            match nxcore::diag::collect(&udid, &bundle, lc.as_deref(), standalone.as_deref(), &out, true).await {
                Ok(c) => {
                    let line = c.log_tail.iter().rev().find(|l| l.contains("[RED]") || l.contains("Break#")).or(c.log_tail.last()).cloned();
                    app.toast(match line {
                        Some(l) => format!("{} files · {}", c.files.len(), l.trim_start_matches("[dm] ")),
                        None => format!("{} files (no log from the game yet)", c.files.len()),
                    });
                    open_path(&c.folder);
                }
                Err(e) => app.toast(format!("Could not read the log: {e:#}")),
            }
        });
    }

    /// Every game's log on the device and the crash reports into <data>/diag/device-<date>, then opens the folder
    fn export_logs(self: &Arc<Self>) {
        let Some(udid) = self.udid() else { return };
        let (lc, names) = {
            let st = self.st.lock().unwrap();
            (st.lc_app.as_ref().map(|l| l.bundle_id.clone()), st.games.iter().map(|g| (g.bundle_id(), g.name.clone())).collect::<Vec<_>>())
        };
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) + local_offset();
        let (y, mo, d) = civil(now / 86400);
        let out = self.store.root().join("diag").join(format!("device-{y}-{mo:02}-{d:02}-{:02}{:02}", now % 86400 / 3600, now % 3600 / 60));
        self.activity(|st: &mut State| -> &mut Activity { &mut st.dev }, "Exporting the logs", move |_app, _say| async move {
            let done = nxcore::diag::collect_all(&udid, lc.as_deref(), &names, &out).await?;
            open_path(&out);
            Ok(if done.is_empty() { format!("No logs on the device yet ({})", out.display()) } else { format!("Exported {} to {}", done.join(", "), out.display()) })
        });
    }

    /// The IPA and the account's certificate into the Downloads folder
    fn export(self: &Arc<Self>, id: u64) {
        let ipa = self.store.ipa(id);
        let name = self.game(id).map(|g| g.name).unwrap_or_else(|| format!("{id:016X}"));
        let dir = export::downloads_dir();
        let app = self.clone();
        self.toast("Saving the IPA…");
        self.rt.spawn(async move {
            let base = export::file_name(&name);
            let (ipa2, dir2) = (ipa.clone(), dir.clone());
            let r = tokio::task::spawn_blocking(move || export::copy_into(&ipa2, &dir2, &format!("{base}.ipa"), &|_, _| {})).await;
            let mut msg = match r {
                Ok(Ok(p)) => format!("Saved {}", p.display()),
                Ok(Err(e)) => format!("Could not save: {e:#}"),
                Err(e) => format!("Could not save: {e}"),
            };
            let mut guard = app.session.lock().await;
            if let Some(s) = guard.as_mut() {
                if let Ok((p12, _)) = s.existing_certificate_p12(&app.store, "").await {
                    if let Ok(c) = export::write_certificate(&dir, &format!("{}.p12", export::file_name(s.email())), &p12) {
                        msg.push_str(&format!(" and {}", c.file_name().unwrap_or_default().to_string_lossy()));
                    }
                }
            }
            app.toast(msg);
        });
    }

    fn lc_install(self: &Arc<Self>, memory: bool) {
        if self.need_sign_in() {
            return;
        }
        let Some(udid) = self.udid() else { return };
        self.activity(|st: &mut State| -> &mut Activity { &mut st.lc }, "Installing LiveContainer", move |app, say| async move {
            let mut guard = app.session.lock().await;
            let s = guard.as_mut().ok_or_else(|| anyhow::anyhow!("not signed in"))?;
            let r = lc_usb::reinstall(s, &app.store, &udid, memory, say).await;
            drop(guard);
            app.refresh_account();
            r
        });
    }

    fn install_ipa(self: &Arc<Self>, ipa: PathBuf) {
        if self.need_sign_in() {
            return;
        }
        let Some(udid) = self.udid() else { return };
        let memory = self.settings.lock().unwrap().increased_memory_limit;
        let name = ipa.file_name().unwrap_or_default().to_string_lossy().into_owned();
        self.activity(|st: &mut State| -> &mut Activity { &mut st.dev }, &format!("Installing {name}"), move |app, say| async move {
            let mut guard = app.session.lock().await;
            let s = guard.as_mut().ok_or_else(|| anyhow::anyhow!("not signed in"))?;
            let s2 = say.clone();
            apple::install(s, &udid, &ipa, memory, Arc::new(move |f| s2(format!("Installing… {:.0}%", f * 100.0), f))).await?;
            drop(guard);
            app.refresh_account();
            Ok(format!("Installed {name}"))
        });
    }

    fn load_app_ids(self: &Arc<Self>) {
        self.st.lock().unwrap().app_ids_loading = true;
        self.render();
        self.refresh_account();
    }

    fn app_id_more(self: &Arc<Self>, identifier: String) {
        self.activity(|st: &mut State| -> &mut Activity { &mut st.dev }, &format!("More RAM for {identifier}"), move |app, _say| async move {
            let mut guard = app.session.lock().await;
            let s = guard.as_mut().ok_or_else(|| anyhow::anyhow!("not signed in"))?;
            s.add_memory_limit(&identifier).await?;
            drop(guard);
            app.refresh_account();
            Ok(format!("More RAM added to {identifier}: reinstall that app so it takes effect"))
        });
    }

    // ---------------------------------------------------------------- settings

    /// The Apple tools' state (the Metal compiler's version runs it under wine: asked once, on a worker thread)
    fn refresh_tools(self: &Arc<Self>) {
        let app = self.clone();
        std::thread::spawn(move || {
            let settings = app.settings.lock().unwrap().clone();
            let sdk = app.store.sdk().map(|s| s.file_name().unwrap_or_default().to_string_lossy().trim_end_matches(".sdk").to_string());
            let metal = metal::installed(&app.store).then(|| {
                let cached = app.st.lock().unwrap().metal_version.clone();
                let v = cached.unwrap_or_else(|| {
                    let v = metal::version(&app.store).unwrap_or_else(|e| e.to_string());
                    app.st.lock().unwrap().metal_version = Some(v.clone());
                    v
                });
                v.replace("Apple metal version ", "").split(' ').next().unwrap_or("").to_string()
            });
            let scripts = match &settings.nxnative_repo {
                Some(p) => p.display().to_string(),
                None => "Bundled".into(),
            };
            {
                let mut st = app.st.lock().unwrap();
                st.sdk = sdk;
                st.metal = metal;
                st.scripts = scripts;
            }
            app.render();
        });
    }

    // ---------------------------------------------------------------- updates

    fn update_progress(self: &Arc<Self>, busy: bool, label: impl Into<String>, fraction: f32) {
        {
            let mut st = self.st.lock().unwrap();
            st.update_busy = busy;
            st.update_label = label.into();
            st.update_fraction = fraction;
        }
        self.render();
    }

    /// What the latest release has, in a line
    fn describe(u: &nxcore::update::Status) -> String {
        match (&u.client, u.bundle) {
            (Some(c), Some(b)) => format!("App {c} and bundle {b} available"),
            (Some(c), None) => format!("App {c} available"),
            (None, Some(b)) if u.bundle_needs_client => format!("Bundle {b} needs app {} or newer", u.manifest.bundle.min_client),
            (None, Some(b)) => format!("Bundle {b} available"),
            (None, None) => format!("Up to date ({})", u.manifest.tag),
        }
    }

    /// Reads the latest release's signed manifest. `quiet` (at start and every two minutes) shows only the card when there is something new;
    /// the Check button also says when there is nothing, or what failed
    fn check_updates(self: &Arc<Self>, quiet: bool) {
        {
            let st = self.st.lock().unwrap();
            if st.update_installing || st.restart_exe.is_some() {
                return;   // installing, or installed and waiting for a restart: nothing to look for
            }
        }
        let app = self.clone();
        if !quiet {
            self.update_progress(true, "Checking the latest release…", 0.0);
        }
        self.rt.spawn(async move {
            app.st.lock().unwrap().bundle_version = nxcore::bundle::active_version(&app.store);
            match nxcore::update::check(&app.store).await {
                Ok(u) => {
                    let line = Self::describe(&u);
                    let unseen = nxcore::update::unseen_changes(&app.store, &u.manifest);
                    {
                        let mut st = app.st.lock().unwrap();
                        // this app's release has changes not shown yet (it was updated by an app that kept none): shown once
                        if !unseen.is_empty() && !st.whats_new_open {
                            st.whats_new = unseen;
                            st.whats_new_installed = true;
                            st.whats_new_open = true;
                        }
                    }
                    app.st.lock().unwrap().update = Some(u);
                    if quiet {
                        app.st.lock().unwrap().update_label = line;
                        app.render();
                    } else {
                        app.update_progress(false, line, 0.0);
                    }
                }
                Err(e) => {
                    tracing::warn!("update check: {e:#}");
                    if !quiet {
                        app.update_progress(false, format!("Failed: {e:#}"), 0.0);
                    }
                }
            }
        });
    }

    /// The checks: now, then every two minutes while the setting is on
    fn watch_updates(self: &Arc<Self>) {
        let app = self.clone();
        self.rt.spawn(async move {
            loop {
                if app.settings.lock().unwrap().check_updates {
                    app.check_updates(true);
                }
                tokio::time::sleep(std::time::Duration::from_secs(120)).await;
            }
        });
    }

    /// Later (or OK once a bundle is installed): the card goes until a newer release
    fn dismiss_update(self: &Arc<Self>) {
        {
            let mut st = self.st.lock().unwrap();
            if !st.update_done.is_empty() && st.restart_exe.is_none() {
                st.update_done.clear();
            } else if let Some(u) = &st.update {
                st.update_dismissed = u.manifest.tag.clone();
            }
        }
        self.render();
    }

    /// Installs what the checked release has, with its progress on the card: the bundle (used from the next build), then the app (from a restart)
    fn install_updates(self: &Arc<Self>) {
        let Some(u) = self.st.lock().unwrap().update.clone() else { return };
        {
            let mut st = self.st.lock().unwrap();
            if st.update_installing {
                return;
            }
            st.update_installing = true;
            st.update_step = "Starting…".into();
            st.update_fraction = 0.0;
            st.update_done.clear();
        }
        self.render();
        let app = self.clone();
        self.rt.spawn(async move {
            let bar = |what: &'static str| -> nxcore::update::Progress {
                let (a, last) = (app.clone(), Arc::new(std::sync::atomic::AtomicU32::new(u32::MAX)));
                Arc::new(move |step, d, t| {
                    let pct = if t > 0 { (d * 100 / t) as u32 } else { 0 };
                    let key = pct + if step == "unpack" { 1000 } else { 0 };
                    if last.swap(key, Ordering::Relaxed) != key {
                        {
                            let mut st = a.st.lock().unwrap();
                            st.update_step = format!("{} {what} · {pct}%", if step == "unpack" { "Installing" } else { "Downloading" });
                            st.update_fraction = pct as f32 / 100.0;
                        }
                        a.render();
                    }
                })
            };
            let fail = |e: anyhow::Error| {
                {
                    let mut st = app.st.lock().unwrap();
                    st.update_installing = false;
                    st.update_done = format!("The update failed: {e:#}");
                }
                app.render();
            };
            let changes = nxcore::update::changes_since(&u.manifest, nxcore::update::installed_release(&app.store));
            let mut done = vec![];
            if u.bundle.is_some() && !u.bundle_needs_client {
                match nxcore::update::install_bundle(&app.store, &u, bar("the bundle")).await {
                    Ok(_) => done.push(format!("bundle {}", u.manifest.bundle.version)),
                    Err(e) => return fail(e),
                }
            }
            let mut exe = None;
            if let Some(v) = &u.client {
                match nxcore::update::install_client(&app.store, &u, "nxnativev2", bar("the app")).await {
                    Ok(p) => {
                        done.push(format!("app {v}"));
                        exe = Some(p);
                    }
                    Err(e) => return fail(e),
                }
            }
            {
                let mut st = app.st.lock().unwrap();
                st.update_installing = false;
                st.update = None;
                st.bundle_version = nxcore::bundle::active_version(&app.store);
                st.update_done = format!("Installed {}{}", done.join(" and "), if exe.is_some() { ": restart to use it" } else { ": used from the next build" });
                st.update_label = st.update_done.clone();
                // what's new: kept for after the restart (a new app), shown now when only the bundle changed
                if let Err(e) = nxcore::update::save_whats_new(&app.store, &changes) {
                    tracing::warn!("what's new: {e:#}");
                }
                if exe.is_none() && !changes.is_empty() {
                    st.whats_new = changes;
                    st.whats_new_installed = true;
                    st.whats_new_open = true;
                }
                st.restart_exe = exe;
            }
            app.render();
        });
    }

    /// Starts the installed app again (the path it had before the update: on Linux the running file is the replaced one) and leaves
    fn restart_app(self: &Arc<Self>) {
        let Some(exe) = self.st.lock().unwrap().restart_exe.clone() else { return };
        tracing::info!(exe = %exe.display(), "update: restarting");
        match std::process::Command::new(&exe).args(std::env::args_os().skip(1)).spawn() {
            Ok(_) => {
                let _ = slint::quit_event_loop();
                std::process::exit(0);
            }
            Err(e) => {
                self.st.lock().unwrap().update_done = format!("Could not start {}: {e}; start it by hand", exe.display());
                self.render();
            }
        }
    }

    fn tools_progress(self: &Arc<Self>, busy: bool, label: impl Into<String>, fraction: f32) {
        {
            let mut st = self.st.lock().unwrap();
            st.tools_busy = busy;
            st.tools_label = label.into();
            st.tools_fraction = fraction;
        }
        self.render();
    }
}

/// A game's activity: its standalone install, or its copy into LiveContainer
fn game_slot(st: &mut State, id: u64, copy: bool) -> &mut Activity {
    if copy { st.copies.entry(id).or_default() } else { st.standalone.entry(id).or_default() }
}

/// A log as the window shows it: its lines as one text
fn lines(v: &[String]) -> SharedString {
    v.join("\n").into()
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn open_path(p: &Path) {
    let cmd = if cfg!(windows) { "explorer" } else { "xdg-open" };
    let _ = std::process::Command::new(cmd).arg(p).spawn();
}

/// Development: each page as a PNG (`NXNATIVE_SNAPSHOT`), one after the other, then quits
fn snapshots(app: Arc<App>, dir: PathBuf, page: i32) {
    if page > 3 {
        let _ = slint::quit_event_loop();
        return;
    }
    app.st.lock().unwrap().page = page;
    if std::env::var_os("NXNATIVE_SNAPSHOT_LOGS").is_some() {
        // sample lines in the selected game's copy and the device's activity, with their logs open
        let mut st = app.st.lock().unwrap();
        let id = st.games.get(st.selected.max(0) as usize).map(|g| g.id).unwrap_or(0);
        let sample: Vec<String> = (1..=30).map(|i| format!("[18:{:02}:{:02}] sample line {i}: a long log line that should wrap across the width of the page instead of being cut off at the edge of a narrow card", i / 2, i)).collect();
        st.copies.entry(id).or_default().log = sample.clone();
        st.dev.log = sample;
        drop(st);
        if let Some(w) = app.ui.upgrade() {
            w.set_lcd_log_open(true);
            w.set_d_log_open(true);
        }
    }
    if let Some(w) = app.ui.upgrade() {
        app.draw(&w);
    }
    slint::Timer::single_shot(Duration::from_millis(1500), move || {
        if let Some(w) = app.ui.upgrade() {
            app.draw(&w);
            if let Ok(px) = w.window().take_snapshot() {
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join(format!("page-{page}.png"));
                let rgba: Vec<u8> = px.as_bytes().to_vec();
                if let Ok(img) = image_png(&path, px.width(), px.height(), &rgba) {
                    let _ = img;
                }
            }
        }
        snapshots(app, dir, page + 1);
    });
}

fn image_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<()> {
    let f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(f, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(rgba)?;
    Ok(())
}

/// Picks files with the system dialog (off the UI thread)
fn pick(app: &Arc<App>, title: &'static str, filters: &'static [(&'static str, &'static [&'static str])], multiple: bool, then: impl FnOnce(Vec<PathBuf>) + Send + 'static) {
    app.rt.spawn(async move {
        let mut d = rfd::AsyncFileDialog::new().set_title(title);
        for (name, ext) in filters {
            d = d.add_filter(*name, ext);
        }
        let files: Vec<PathBuf> = if multiple {
            d.pick_files().await.unwrap_or_default().into_iter().map(|f| f.path().to_path_buf()).collect()
        } else {
            d.pick_file().await.into_iter().map(|f| f.path().to_path_buf()).collect()
        };
        if !files.is_empty() {
            then(files);
        }
    });
}

fn main() -> Result<()> {
    let settings = Settings::load()?;
    let store = Store::new(settings.data_dir()?)?;
    nxcore::update::cleanup(&store);
    std::fs::create_dir_all(store.tmp_dir())?;
    nxcore::logging::init(&store);
    // SAFETY: before any other thread exists. The IPA is unpacked into the temp folder to be signed (6+ GB; /tmp may be a RAM disk).
    // On Wayland, winit cannot receive dropped files: use XWayland when it is there (NXNATIVE_WAYLAND=1 keeps Wayland).
    unsafe {
        for k in ["TMPDIR", "TMP", "TEMP"] {
            std::env::set_var(k, store.tmp_dir());
        }
        if cfg!(target_os = "linux")
            && std::env::var_os("WAYLAND_DISPLAY").is_some()
            && std::env::var_os("DISPLAY").is_some()
            && std::env::var("NXNATIVE_WAYLAND").as_deref() != Ok("1")
        {
            std::env::remove_var("WAYLAND_DISPLAY");
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let ui = AppWindow::new()?;
    let app = Arc::new(App {
        ui: ui.as_weak(),
        rt: rt.handle().clone(),
        settings: Mutex::new(settings.clone()),
        store,
        st: Mutex::new(State::default()),
        session: tokio::sync::Mutex::new(None),
        tfa: Mutex::new(None),
        certs: Mutex::new(None),
        dropped: Mutex::new(vec![]),
        lc_copy: Mutex::new(None),
        device_refreshing: AtomicBool::new(false),
        render_queued: AtomicBool::new(false),
        device_gen: AtomicU64::new(0),
    });
    if let Some(id) = &settings.apple_id {
        ui.set_apple_id(id.clone().into());
        ui.set_has_saved_password(apple::credentials::load(id).is_some());
    }
    ui.set_more_ram(settings.increased_memory_limit);
    ui.set_show_fps(settings.show_fps);
    ui.set_debug_builds(settings.debug_builds);
    ui.set_u_check_on_start(settings.check_updates);
    {
        let mut st = app.st.lock().unwrap();
        st.bundle_version = nxcore::bundle::active_version(&app.store);
        // an update installed before the restart left its changes: shown once
        st.whats_new = nxcore::update::whats_new(&app.store);
        st.whats_new_installed = !st.whats_new.is_empty();
        st.whats_new_open = st.whats_new_installed;
        if st.whats_new_installed {
            nxcore::update::mark_seen(&app.store);
        }
    }
    app.watch_updates();   // at start, then every two minutes (Settings › Updates › Check for updates)

    // files dropped on the window (several arrive as one event each: gathered, then imported together)
    {
        use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
        let app = app.clone();
        ui.window().on_winit_window_event(move |_w, ev| {
            match ev {
                WindowEvent::HoveredFile(_) => app.ui.upgrade().map(|u| u.set_dropping(true)).unwrap_or(()),
                WindowEvent::HoveredFileCancelled => app.ui.upgrade().map(|u| u.set_dropping(false)).unwrap_or(()),
                WindowEvent::DroppedFile(p) => {
                    app.dropped.lock().unwrap().push(p.clone());
                    if let Some(u) = app.ui.upgrade() {
                        u.set_dropping(false);
                    }
                    let app = app.clone();
                    slint::Timer::single_shot(Duration::from_millis(200), move || {
                        let files = std::mem::take(&mut *app.dropped.lock().unwrap());
                        if !files.is_empty() {
                            let target = app.import_target();
                            app.import(files, target);
                        }
                    });
                }
                _ => {}
            }
            EventResult::Propagate
        });
    }

    let a = app.clone();
    ui.on_select_game(move |i| {
        {
            let mut st = a.st.lock().unwrap();
            st.page = 0;
            st.selected = i;
        }
        a.render();
    });
    let a = app.clone();
    ui.on_select_page(move |p| {
        a.st.lock().unwrap().page = p;
        a.render();
        match p {
            1 | 2 => a.refresh_device(true),
            3 => a.refresh_tools(),
            _ => {}
        }
    });
    let a = app.clone();
    ui.on_add_files(move || {
        let target = a.import_target();
        let a2 = a.clone();
        pick(&a, "Game files, keys and apps", &[("Switch games, keys and iOS apps", &["nsp", "xci", "keys", "ipa"])], true, move |files| a2.import(files, target));
    });
    let a = app.clone();
    ui.on_remove_game(move || {
        if let Some(id) = a.selected_id() {
            let name = a.game(id).map(|g| g.name).unwrap_or_default();
            match games::remove(&a.store, id) {
                Ok(()) => a.toast(format!("Removed {name}")),
                Err(e) => a.toast(format!("Could not remove: {e:#}")),
            }
            a.st.lock().unwrap().selected = 0;
            a.refresh_games(None);
        }
    });
    let a = app.clone();
    ui.on_build(move || {
        if let Some(id) = a.selected_id() {
            a.build(id);
        }
    });
    let a = app.clone();
    ui.on_cancel_build(move || a.cancel_build());
    let a = app.clone();
    ui.on_sa_install(move || {
        if let Some(id) = a.selected_id() {
            a.sa_install(id);
        }
    });
    let a = app.clone();
    ui.on_sa_remove(move || {
        if let Some(id) = a.selected_id() {
            a.sa_remove(id);
        }
    });
    let a = app.clone();
    ui.on_lcd_copy(move || {
        if let Some(id) = a.selected_id() {
            a.lcd_copy(id);
        }
    });
    let a = app.clone();
    ui.on_lcd_cancel(move || a.cancel_copy());
    let a = app.clone();
    ui.on_lcd_share(move || {
        if let Some(id) = a.selected_id() {
            a.share(Some(id));
        }
    });
    let a = app.clone();
    ui.on_lcd_save(move || {
        if let Some(id) = a.selected_id() {
            a.export(id);
        }
    });
    let a = app.clone();
    ui.on_stop_share(move || a.stop_share());
    let a = app.clone();
    ui.on_export_logs(move || a.export_logs());
    let a = app.clone();
    ui.on_game_shaders_changed(move |on| {
        if let Some(id) = a.selected_id() {
            let mut s = a.settings.lock().unwrap();
            s.set_shaders_on_device(id, on);
            let _ = s.save();
        }
    });
    let a = app.clone();
    ui.on_device_log(move || {
        if let Some(id) = a.selected_id() {
            a.device_log(id);
        }
    });
    let a = app.clone();
    ui.on_lc_install(move || {
        let memory = a.settings.lock().unwrap().increased_memory_limit;
        a.lc_install(memory);
    });
    let a = app.clone();
    ui.on_lc_certificate(move || {
        if !a.need_sign_in() {
            a.share(None);
        }
    });
    let a = app.clone();
    ui.on_lc_copy_game(move |i| {
        let id = a.st.lock().unwrap().games.get(i.max(0) as usize).map(|g| g.id);
        if let Some(id) = id {
            a.lcd_copy(id);
        }
    });
    let a = app.clone();
    ui.on_select_device(move |i| {
        a.st.lock().unwrap().device_index = i.max(0) as usize;
        a.refresh_device(true);
    });
    let a = app.clone();
    ui.on_sign_in(move || {
        if let Some(w) = a.ui.upgrade() {
            a.sign_in(w.get_apple_id().to_string(), w.get_password().to_string(), w.get_remember());
        }
    });
    let a = app.clone();
    ui.on_sign_out(move || {
        let a2 = a.clone();
        a.rt.spawn(async move {
            *a2.session.lock().await = None;
            {
                let mut st = a2.st.lock().unwrap();
                st.account = None;
                st.app_ids.clear();
            }
            a2.render();
        });
    });
    let a = app.clone();
    ui.on_forget_password(move || {
        if let Some(w) = a.ui.upgrade() {
            apple::credentials::forget(&w.get_apple_id());
            w.set_has_saved_password(false);
        }
    });
    let a = app.clone();
    ui.on_install_ipa(move || {
        let a2 = a.clone();
        pick(&a, "IPA to install", &[("iOS app", &["ipa"])], false, move |f| a2.install_ipa(f[0].clone()));
    });
    let a = app.clone();
    ui.on_load_app_ids(move || a.load_app_ids());
    let a = app.clone();
    ui.on_app_id_more(move |id| a.app_id_more(id.to_string()));
    let a = app.clone();
    ui.on_pick_xip(move || {
        let a2 = a.clone();
        pick(&a, "Xcode .xip", &[("Xcode archive", &["xip"])], false, move |f| {
            let app = a2.clone();
            std::thread::spawn(move || {
                app.tools_progress(true, "Taking the iOS SDK from Xcode", 0.0);
                let a3 = app.clone();
                let r = xip::extract_ios_sdk(&f[0], &app.store.sdk_dir(), &move |d, t| {
                    let mut st = a3.st.lock().unwrap();
                    st.tools_fraction = d as f32 / t.max(1) as f32;
                    drop(st);
                    a3.render();
                });
                match r {
                    Ok(p) => app.tools_progress(false, format!("{} ready", p.file_name().unwrap_or_default().to_string_lossy()), 1.0),
                    Err(e) => app.tools_progress(false, format!("Failed: {e:#}"), 0.0),
                }
                app.refresh_tools();
            });
        });
    });
    let a = app.clone();
    ui.on_pick_metal(move || {
        let a2 = a.clone();
        pick(&a, "Metal Developer Tools for Windows installer", &[("Installer", &["exe"])], false, move |f| {
            let app = a2.clone();
            std::thread::spawn(move || {
                app.tools_progress(true, "Installing the Metal compiler", 0.0);
                match metal::install(&app.store, &f[0]) {
                    Ok(v) => {
                        app.st.lock().unwrap().metal_version = Some(v);
                        app.tools_progress(false, "Metal compiler ready", 1.0);
                    }
                    Err(e) => app.tools_progress(false, format!("Failed: {e:#}"), 0.0),
                }
                app.refresh_tools();
            });
        });
    });
    let a = app.clone();
    ui.on_pick_repo(move || {
        let a2 = a.clone();
        a.rt.spawn(async move {
            if let Some(d) = rfd::AsyncFileDialog::new().set_title("nxnative checkout").pick_folder().await {
                {
                    let mut s = a2.settings.lock().unwrap();
                    s.nxnative_repo = Some(d.path().to_path_buf());
                    let _ = s.save();
                }
                a2.refresh_tools();
                a2.refresh_games(None);
            }
        });
    });
    let a = app.clone();
    ui.on_open_data(move || open_path(a.store.root()));
    let a = app.clone();
    ui.on_open_logs(move || open_path(&a.store.root().join("logs")));
    let a = app.clone();
    ui.on_check_updates(move || a.check_updates(false));
    let a = app.clone();
    ui.on_install_updates(move || a.install_updates());
    let a = app.clone();
    ui.on_dismiss_update(move || a.dismiss_update());
    let a = app.clone();
    ui.on_restart_app(move || a.restart_app());
    let a = app.clone();
    ui.on_show_whats_new(move || a.show_whats_new());
    let a = app.clone();
    ui.on_close_whats_new(move || a.close_whats_new());
    let a = app.clone();
    ui.on_settings_changed(move || {
        if let Some(w) = a.ui.upgrade() {
            let mut s = a.settings.lock().unwrap();
            s.increased_memory_limit = w.get_more_ram();
            s.show_fps = w.get_show_fps();
            s.debug_builds = w.get_debug_builds();
            s.check_updates = w.get_u_check_on_start();
            let _ = s.save();
        }
    });
    let a = app.clone();
    ui.on_tfa_submit(move || {
        if let Some(w) = a.ui.upgrade() {
            a.answer_tfa(apple::TwoFactorAnswer::Code(w.get_tfa_code().to_string()));
        }
    });
    let a = app.clone();
    ui.on_tfa_sms(move |id| a.answer_tfa(apple::TwoFactorAnswer::SmsTo(id.max(1) as u32)));
    let a = app.clone();
    ui.on_tfa_devices(move || a.answer_tfa(apple::TwoFactorAnswer::ToDevices));
    let a = app.clone();
    ui.on_tfa_resend(move || a.answer_tfa(apple::TwoFactorAnswer::Resend));
    let a = app.clone();
    ui.on_tfa_cancel(move || a.answer_tfa(apple::TwoFactorAnswer::Abort));
    let a = app.clone();
    ui.on_revoke(move |serial| {
        if let Some(tx) = a.certs.lock().unwrap().take() {
            let _ = tx.send(vec![serial.to_string()]);
        }
        if let Some(w) = a.ui.upgrade() {
            w.set_certs_open(false);
        }
    });
    let a = app.clone();
    ui.on_certs_cancel(move || {
        if let Some(tx) = a.certs.lock().unwrap().take() {
            let _ = tx.send(vec![]);
        }
        if let Some(w) = a.ui.upgrade() {
            w.set_certs_open(false);
        }
    });

    // open on the games when the Apple tools are ready, else on the settings
    let ready = pipeline::check(&settings, &app.store).iter().all(|r| r.ok);
    app.st.lock().unwrap().page = if ready { 0 } else { 3 };
    app.refresh_games(None);
    app.refresh_tools();
    app.refresh_device(true);
    // devices come and go: a light look every few seconds (the details only when the list changes)
    {
        let a = app.clone();
        app.rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                a.refresh_device(false);
            }
        });
    }
    // development: NXNATIVE_SNAPSHOT=<folder> renders each page to a PNG there and quits (no sign-in)
    let snapshot = std::env::var_os("NXNATIVE_SNAPSHOT").map(PathBuf::from);
    if let Some(dir) = snapshot.clone() {
        let a = app.clone();
        slint::Timer::single_shot(Duration::from_secs(5), move || snapshots(a, dir, 0));
    }
    // signed in again with the password saved in the keyring (Apple may still ask for a code, in the dialog)
    if let Some(id) = settings.apple_id.clone().filter(|id| apple::credentials::load(id).is_some() && snapshot.is_none()) {
        app.sign_in(id, String::new(), true);
    }
    ui.run()?;
    // a build still running stops with the app (it runs in a process group of its own, which closing the app would not reach)
    if let Some((_, c)) = app.st.lock().unwrap().running.take() {
        c.cancel();
    }
    drop(rt);
    Ok(())
}
