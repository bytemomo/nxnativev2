//! nxnativev2: the desktop app. The window (ui/app.slint) on the main thread; builds, sign-in and installing on a tokio runtime and
//! worker threads, which update the window through `invoke_from_event_loop`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures::FutureExt;
use nxcore::{apple, export, games, metal, nsp::Kind, pipeline, settings::Settings, store::Store, xip};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};
use tokio::sync::oneshot;

slint::include_modules!();

/// The visible state of a game's build (kept per game, so switching games keeps each one's progress)
#[derive(Default, Clone)]
struct BuildView {
    label: String,
    fraction: f32,
    indeterminate: bool,
    detail: String,
    log: String,
}

struct App {
    ui: Weak<AppWindow>,
    rt: tokio::runtime::Handle,
    settings: Mutex<Settings>,
    store: Store,
    games: Mutex<Vec<games::Game>>,
    builds: Mutex<HashMap<u64, BuildView>>,
    running: Mutex<Option<(u64, pipeline::Canceller)>>,
    session: tokio::sync::Mutex<Option<apple::Session>>,
    devices: Mutex<Vec<apple::Device>>,
    tfa: Mutex<Option<oneshot::Sender<apple::TwoFactorAnswer>>>,
    certs: Mutex<Option<oneshot::Sender<Vec<String>>>>,
    dropped: Mutex<Vec<PathBuf>>,
    share: Mutex<Option<nxcore::serve::Server>>,
    /// the Metal compiler's version line (running it takes seconds under wine): asked once, again after an install
    metal_version: Mutex<Option<String>>,
    app_ids: Mutex<Vec<apple::AppIdInfo>>,
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

fn hms(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

impl App {
    fn selected_game(&self, ui: &AppWindow) -> Option<games::Game> {
        let i = ui.get_selected();
        (i >= 0).then(|| self.games.lock().unwrap().get(i as usize).cloned()).flatten()
    }

    fn status(&self, msg: impl Into<String>) {
        let msg: SharedString = msg.into().into();
        on_ui(&self.ui, move |w| w.set_status(msg));
    }

    /// Reloads the game list (keeping the selection by title ID) and the selected game's panel
    fn refresh_games(self: &Arc<Self>, select: Option<u64>) {
        let repo = pipeline::repo(&self.settings.lock().unwrap(), &self.store);
        let list = games::list(&self.store, repo.as_deref()).unwrap_or_default();
        let app = self.clone();
        on_ui(&self.ui, move |w| {
            let keep = select.or_else(|| app.selected_game(w).map(|g| g.id));
            let items: Vec<GameItem> = list
                .iter()
                .map(|g| {
                    let mut parts = vec![];
                    parts.push(if g.has_base() { g.newest_update().map_or("base game".into(), |v| format!("v{v}")) } else { "no base game".into() });
                    if g.dlc_count() > 0 {
                        parts.push(format!("{} DLC", g.dlc_count()));
                    }
                    if !g.has_keys {
                        parts.push("no keys".into());
                    }
                    if g.ipa.is_some() {
                        parts.push("IPA ready".into());
                    }
                    GameItem { id: format!("{:016X}", g.id).into(), name: g.name.clone().into(), subtitle: parts.join(" · ").into(), built: g.ipa.is_some() }
                })
                .collect();
            let idx = keep.and_then(|id| list.iter().position(|g| g.id == id));
            *app.games.lock().unwrap() = list;
            w.set_games(model(items));
            match idx {
                Some(i) => w.set_selected(i as i32),
                None if w.get_selected() >= app.games.lock().unwrap().len() as i32 => w.set_selected(-1),
                None => {}
            }
            app.show_selected(w);
        });
    }

    /// Fills the main area for the selected game (UI thread)
    fn show_selected(self: &Arc<Self>, w: &AppWindow) {
        let Some(g) = self.selected_game(w) else {
            if w.get_selected() == -1 {
                self.show_tools(w);
            }
            return;
        };
        w.set_game_name(g.name.clone().into());
        w.set_game_id(format!("{:016X}", g.id).into());
        w.set_game_badge(if g.supported { "supported game".into() } else { "no game profile yet: the app may not start".into() });
        let mut lines: Vec<Line> = g
            .files
            .iter()
            .map(|f| Line {
                title: match f.kind {
                    Kind::Base => "Base game".into(),
                    Kind::Update => format!("Update {}", f.version.map_or("(no version tag)".into(), |v| format!("v{v}"))).into(),
                    Kind::Dlc => format!("DLC {:016X}", f.title_id).into(),
                },
                detail: format!("{} · {}", f.path.file_name().unwrap_or_default().to_string_lossy(), size(f.size)).into(),
                ok: true,
            })
            .collect();
        if !g.has_base() {
            lines.insert(0, Line { title: "Base game".into(), detail: "missing: add the base game's NSP".into(), ok: false });
        }
        lines.push(Line {
            title: "prod.keys".into(),
            detail: if g.has_keys { "present".into() } else { "missing: add your console's prod.keys".into() },
            ok: g.has_keys,
        });
        w.set_game_files(model(lines));
        w.set_game_missing(g.missing().join(", ").into());
        w.set_ipa_info(
            g.ipa
                .as_ref()
                .map(|p| format!("IPA ready: {} ({})", p.display(), size(std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))))
                .unwrap_or_default()
                .into(),
        );
        let running = self.running.lock().unwrap().as_ref().map(|(id, _)| *id);
        let view = self.builds.lock().unwrap().get(&g.id).cloned().unwrap_or_default();
        w.set_building(running == Some(g.id));
        w.set_build_label(view.label.into());
        w.set_build_fraction(view.fraction);
        w.set_build_indeterminate(view.indeterminate);
        w.set_build_detail(view.detail.into());
        w.set_build_log(view.log.into());
    }

    /// Fills the Apple tools page. The checks run commands (the Metal compiler under wine takes seconds): they run on a worker thread,
    /// and the page is filled when they are done
    fn show_tools(self: &Arc<Self>, w: &AppWindow) {
        w.set_data_path(self.store.root().display().to_string().into());
        let app = self.clone();
        std::thread::spawn(move || {
            let settings = app.settings.lock().unwrap().clone();
            let reqs: Vec<(String, String, bool)> = pipeline::check(&settings, &app.store).into_iter().map(|r| (r.name.to_string(), r.detail, r.ok)).collect();
            let sdk = match app.store.sdk() {
                Some(s) => format!("Installed: {}", s.file_name().unwrap_or_default().to_string_lossy()),
                None => "Not installed. Download Xcode (the .xip, e.g. \"Xcode 26.6 Apple silicon.xip\") and choose it here: only the iPhone SDK and Swift resources are taken from it (about 280 MB).".into(),
            };
            let metal = if metal::installed(&app.store) {
                let cached = app.metal_version.lock().unwrap().clone();
                let v = cached.unwrap_or_else(|| {
                    let v = metal::version(&app.store).unwrap_or_else(|e| e.to_string());
                    *app.metal_version.lock().unwrap() = Some(v.clone());
                    v
                });
                format!("Installed: {v}")
            } else if cfg!(windows) {
                "Not installed. Download \"Metal Developer Tools for Windows\" and choose its installer here.".into()
            } else {
                "Not installed. Download \"Metal Developer Tools for Windows\" and choose its installer here: it is installed into a wine prefix of this app.".into()
            };
            let repo = pipeline::repo(&settings, &app.store).map(|p| p.display().to_string()).unwrap_or_default();
            on_ui(&app.ui, move |w| {
                w.set_requirements(model(reqs.into_iter().map(|(t, d, ok)| Line { title: t.into(), detail: d.into(), ok }).collect()));
                w.set_sdk_state(sdk.into());
                w.set_metal_state(metal.into());
                w.set_repo_path(repo.into());
            });
        });
    }

    fn update_build(self: &Arc<Self>, id: u64, f: impl FnOnce(&mut BuildView)) {
        let view = {
            let mut b = self.builds.lock().unwrap();
            let v = b.entry(id).or_default();
            f(v);
            v.clone()
        };
        let app = self.clone();
        on_ui(&self.ui, move |w| {
            if app.selected_game(w).map(|g| g.id) == Some(id) {
                w.set_build_label(view.label.into());
                w.set_build_fraction(view.fraction);
                w.set_build_indeterminate(view.indeterminate);
                w.set_build_detail(view.detail.into());
                if w.get_show_log() {
                    w.set_build_log(view.log.into());
                }
            }
        });
    }

    fn import(self: &Arc<Self>, paths: Vec<PathBuf>, target: Option<u64>) {
        let app = self.clone();
        std::thread::spawn(move || {
            app.status("Importing…");
            let progress = |d: u64, t: u64| app.status(format!("Importing… {}%", d * 100 / t.max(1)));
            match games::import(&app.store, &paths, target, &progress) {
                Ok(r) => {
                    let mut msg = format!("Imported {} file(s)", r.imported.len());
                    for (p, why) in &r.skipped {
                        msg.push_str(&format!("\nSkipped {}: {why}", p.file_name().unwrap_or_default().to_string_lossy()));
                    }
                    app.status(msg);
                    app.refresh_games(r.games.first().copied());
                }
                Err(e) => app.status(format!("Import failed: {e:#}")),
            }
        });
    }

    /// Downloads what the build needs and is missing (nxnative scripts, Python, .NET), then builds
    fn build(self: &Arc<Self>, id: u64) {
        let settings = self.settings.lock().unwrap().clone();
        let app = self.clone();
        on_ui(&self.ui, |w| w.set_building(true));
        self.update_build(id, |v| *v = BuildView { label: "Preparing the build tools…".into(), indeterminate: true, ..Default::default() });
        self.rt.spawn(async move {
            let a2 = app.clone();
            let progress = move |d: u64, t: u64, step: &str| {
                let step = step.to_string();
                a2.update_build(id, |v| {
                    v.label = format!("Preparing: {step}");
                    v.indeterminate = t == 0;
                    v.fraction = if t > 0 { d as f32 / t as f32 } else { 0.0 };
                    v.detail = if t > 0 { format!("{:.0}/{:.0} MB", d as f64 / 1e6, t as f64 / 1e6) } else { String::new() };
                });
            };
            match pipeline::prepare(&settings, &app.store, &progress).await {
                Ok(()) => app.start_build(id),
                Err(e) => {
                    app.update_build(id, |v| {
                        v.label = format!("Preparing failed: {e:#}");
                        v.indeterminate = false;
                    });
                    on_ui(&app.ui, |w| w.set_building(false));
                }
            }
        });
    }

    fn start_build(self: &Arc<Self>, id: u64) {
        let settings = self.settings.lock().unwrap().clone();
        let app = self.clone();
        self.update_build(id, |v| *v = BuildView { label: "Starting…".into(), indeterminate: true, ..Default::default() });
        let events = {
            let app = app.clone();
            move |e: pipeline::Event| match e {
                pipeline::Event::Stage { k, n, name } => app.update_build(id, |v| {
                    v.label = format!("[{k}/{n}] {name}");
                    v.fraction = 0.0;
                    v.indeterminate = true;
                    v.detail.clear();
                }),
                pipeline::Event::Progress { name, step, frac, count, eta } => app.update_build(id, |v| {
                    v.label = if step.is_empty() { name } else { format!("{} · {step}", v.label.split(" · ").next().unwrap_or(&name)) };
                    v.indeterminate = frac.is_none();
                    v.fraction = frac.unwrap_or(0.0) as f32;
                    v.detail = [count, eta.map(|e| format!("about {} left", hms(e))).unwrap_or_default()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
                }),
                pipeline::Event::StageEnd { name, ok, summary } => app.update_build(id, |v| {
                    v.log.push_str(&format!("{} {name}{}\n", if ok { "✓" } else { "✗" }, if summary.is_empty() { String::new() } else { format!(": {summary}") }));
                    v.fraction = 1.0;
                }),
                pipeline::Event::Say(m) | pipeline::Event::Log(m) => app.update_build(id, |v| {
                    v.log.push_str(&m);
                    v.log.push('\n');
                }),
            }
        };
        match pipeline::start(&settings, &self.store, id, &pipeline::Options { show_fps: true, ..Default::default() }, events) {
            Ok(handle) => {
                *self.running.lock().unwrap() = Some((id, handle.canceller()));
                on_ui(&self.ui, |w| w.set_building(true));
                std::thread::spawn(move || {
                    let result = handle.wait();
                    *app.running.lock().unwrap() = None;
                    match result {
                        Ok(()) => {
                            app.update_build(id, |v| {
                                v.label = "Done: the IPA is ready".into();
                                v.detail.clear();
                            });
                            app.status("Build finished");
                        }
                        Err(e) => {
                            app.update_build(id, |v| {
                                v.label = format!("Failed: {e}");
                                v.detail = "Show the log for details".into();
                            });
                            app.status("Build failed");
                        }
                    }
                    on_ui(&app.ui, |w| w.set_building(false));
                    app.refresh_games(Some(id));
                });
            }
            Err(e) => {
                self.update_build(id, |v| {
                    v.label = format!("Cannot start: {e:#}");
                    v.indeterminate = false;
                });
                on_ui(&self.ui, |w| w.set_building(false));
            }
        }
    }

    fn refresh_devices(self: &Arc<Self>) {
        let app = self.clone();
        self.rt.spawn(async move {
            let (labels, devs) = match apple::devices().await {
                Ok(d) => (d.iter().map(|d| SharedString::from(format!("{} (iOS {})", d.name, d.version))).collect::<Vec<_>>(), d),
                Err(e) => {
                    app.status(format!("{e:#}"));
                    (vec![], vec![])
                }
            };
            *app.devices.lock().unwrap() = devs;
            on_ui(&app.ui, move |w| {
                let n = labels.len() as i32;
                w.set_device_labels(model(labels));
                if w.get_device_index() >= n {
                    w.set_device_index(0);
                }
            });
        });
    }

    fn device_udid(&self, index: i32) -> Option<String> {
        self.devices.lock().unwrap().get(index.max(0) as usize).map(|d| d.udid.clone())
    }

    fn device_progress(&self, busy: bool, label: impl Into<String>, fraction: f32) {
        let label: SharedString = label.into().into();
        on_ui(&self.ui, move |w| {
            w.set_device_busy(busy);
            w.set_device_label(label);
            w.set_device_fraction(fraction);
        });
    }

    /// Signs and installs any IPA (LiveContainer, for example) with the account
    fn install_other(self: &Arc<Self>, index: i32, ipa: PathBuf) {
        let Some(udid) = self.device_udid(index) else { return };
        let app = self.clone();
        let name = ipa.file_name().unwrap_or_default().to_string_lossy().into_owned();
        self.device_progress(true, format!("Signing and installing {name}…"), 0.0);
        self.rt.spawn(async move {
            let mut guard = app.session.lock().await;
            let result = match guard.as_mut() {
                Some(session) => {
                    let a2 = app.clone();
                    let n2 = name.clone();
                    let progress = Arc::new(move |f: f32| a2.device_progress(true, format!("Installing {n2}… {:.0}%", f * 100.0), f.min(0.99)));
                    let mem = app.settings.lock().unwrap().increased_memory_limit;
                    apple::install(session, &udid, &ipa, mem, progress).await
                }
                None => Err(anyhow::anyhow!("not signed in")),
            };
            drop(guard);
            match result {
                Ok(()) => {
                    app.device_progress(false, format!("Installed {name}. On the device: Settings → General → VPN & Device Management → trust your Apple ID."), 1.0);
                    app.refresh_devices();
                }
                Err(e) => app.device_progress(false, format!("Install failed: {e:#}"), 0.0),
            }
        });
    }

    fn sign_in(self: &Arc<Self>, email: String, password: String, remember: bool) {
        let email = email.trim().to_string();
        let saved = apple::credentials::load(&email);
        let (password, from_keyring) = match (password.is_empty(), saved) {
            (true, Some(p)) => (p, true),
            (_, _) => (password, false),
        };
        if password.is_empty() {
            return;
        }
        let app = self.clone();
        on_ui(&self.ui, |w| w.set_signing_in(true));
        let ask: apple::TwoFactorAsk = {
            let app = app.clone();
            Arc::new(move |p: apple::TwoFactorPrompt| {
                let (tx, rx) = oneshot::channel();
                *app.tfa.lock().unwrap() = Some(tx);
                let mut msg = if p.sms { "Apple sent a code by SMS. Enter it below.".to_string() } else { "Enter the code shown on your iPhone, iPad or Mac.".to_string() };
                msg.push_str(" No pop-up? On a device signed in with this Apple ID: Settings → your name → Sign-In & Security → Get Verification Code.");
                if let Some(e) = &p.last_error {
                    msg = format!("Apple: {e}\n\n{msg}");
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
            // in its own task: a panic inside the sign-in ends as an error message, not as a button stuck on "Signing in…"
            let (store2, email2, password2) = (app.store.clone(), email.clone(), password.clone());
            let result = match tokio::spawn(async move { apple::login(&store2, &settings, &email2, &password2, ask, certs).await }).await {
                Ok(r) => r,
                Err(e) => Err(anyhow::anyhow!("the sign-in stopped unexpectedly: {e}")),
            };
            if let Err(e) = &result {
                tracing::error!("sign-in failed: {e:#}");
            }
            if result.is_err() && from_keyring {
                apple::credentials::forget(&email);
            } else if result.is_ok() && remember && !from_keyring {
                let _ = apple::credentials::save(&email, &password);
            }
            let saved_now = apple::credentials::load(&email).is_some();
            on_ui(&app.ui, move |w| w.set_has_saved_password(saved_now));
            on_ui(&app.ui, |w| {
                w.set_signing_in(false);
                w.set_tfa_open(false);
            });
            match result {
                Ok(session) => {
                    let who = session.email().to_string();
                    *app.session.lock().await = Some(session);
                    {
                        let mut s = app.settings.lock().unwrap();
                        s.apple_id = Some(who.clone());
                        let _ = s.save();
                    }
                    app.status(format!("Signed in as {who}"));
                    on_ui(&app.ui, move |w| {
                        w.set_signed_in_as(who.into());
                        w.set_password("".into());
                    });
                }
                Err(e) => app.status(format!("Sign-in failed: {e:#}")),
            }
        });
    }

    fn answer_tfa(&self, a: apple::TwoFactorAnswer) {
        if let Some(tx) = self.tfa.lock().unwrap().take() {
            let _ = tx.send(a);
        }
        on_ui(&self.ui, |w| w.set_tfa_open(false));
    }

    fn install(self: &Arc<Self>, id: u64, device: usize, memory_limit: bool, into_lc: bool) {
        if into_lc {
            return self.share(id);
        }
        let app = self.clone();
        let Some(udid) = self.devices.lock().unwrap().get(device).map(|d| d.udid.clone()) else {
            self.status("No device selected");
            return;
        };
        let ipa = self.store.ipa(id);
        on_ui(&self.ui, |w| {
            w.set_installing(true);
            w.set_install_label("Signing and installing…".into());
            w.set_install_fraction(0.0);
        });
        self.rt.spawn(async move {
            let mut guard = app.session.lock().await;
            let result = match guard.as_mut() {
                Some(session) => {
                    let ui = app.ui.clone();
                    let progress = Arc::new(move |f: f32| {
                        on_ui(&ui, move |w| {
                            w.set_install_fraction(f.min(0.99));
                            w.set_install_label(format!("Installing… {:.0}%", f * 100.0).into());
                        })
                    });
                    apple::install(session, &udid, &ipa, memory_limit, progress).await
                }
                None => Err(anyhow::anyhow!("not signed in")),
            };
            let label = match &result {
                Ok(()) => "Installed. On the device: Settings → Privacy & Security → Developer Mode on, then Settings → General → VPN & Device Management → trust your Apple ID.".to_string(),
                Err(e) => {
                    tracing::error!("install failed: {e:#}");
                    format!("Install failed: {e:#}")
                }
            };
            on_ui(&app.ui, move |w| {
                w.set_installing(false);
                w.set_install_label(label.into());
                w.set_install_fraction(1.0);
            });
        });
    }
}

impl App {
    /// Shares the game with LiveContainer over Wi-Fi: the account certificate (when signed in) and the IPA on a page of this computer
    fn share(self: &Arc<Self>, id: u64) {
        let ipa = self.store.ipa(id);
        let name = self.games.lock().unwrap().iter().find(|g| g.id == id).map(|g| g.name.clone()).unwrap_or_else(|| format!("{id:016X}"));
        let app = self.clone();
        on_ui(&self.ui, |w| {
            w.set_installing(true);
            w.set_install_label("Starting the share page…".into());
            w.set_install_fraction(0.0);
        });
        self.rt.spawn(async move {
            let certificate = {
                let mut guard = app.session.lock().await;
                match guard.as_mut() {
                    Some(s) => match s.existing_certificate_p12(&app.store, "").await {
                        Ok((p12, _)) => Some(p12),
                        Err(e) => {
                            tracing::warn!("share: no certificate: {e:#}");
                            app.status(format!("No certificate on the page: {e:#}"));
                            None
                        }
                    },
                    None => None,
                }
            };
            let result = nxcore::serve::start(nxcore::serve::Share { name, ipa, certificate }, nxcore::serve::DEFAULT_PORT);
            match result {
                Ok(server) => {
                    let url = server.url.clone();
                    let sent = server.sent.clone();
                    *app.share.lock().unwrap() = Some(server);
                    let qr = nxcore::serve::qr_rgba(&url, 6);
                    on_ui(&app.ui, move |w| {
                        w.set_installing(false);
                        w.set_install_label("".into());
                        if let Some((size, px)) = qr {
                            let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&px, size, size);
                            w.set_share_qr(slint::Image::from_rgba8(buf));
                        }
                        w.set_share_url(url.into());
                        w.set_share_sent("".into());
                        w.set_sharing(true);
                    });
                    // the bytes sent, while the share runs
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        if app.share.lock().unwrap().is_none() {
                            break;
                        }
                        let n = sent.load(std::sync::atomic::Ordering::Relaxed);
                        if n > 0 {
                            let text = format!("Sent so far: {:.2} GB.", n as f64 / 1e9);
                            on_ui(&app.ui, move |w| w.set_share_sent(text.into()));
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("share failed: {e:#}");
                    let label = format!("Cannot share: {e:#}");
                    on_ui(&app.ui, move |w| {
                        w.set_installing(false);
                        w.set_install_label(label.into());
                    });
                }
            }
        });
    }

    /// The account's App IDs, for the Increased Memory Limit
    fn load_app_ids(self: &Arc<Self>) {
        let app = self.clone();
        self.device_progress(true, "Loading the App IDs…", 0.0);
        self.rt.spawn(async move {
            let result = {
                let mut guard = app.session.lock().await;
                match guard.as_mut() {
                    Some(s) => s.app_ids().await,
                    None => Err(anyhow::anyhow!("not signed in")),
                }
            };
            match result {
                Ok(ids) => {
                    let labels: Vec<SharedString> = ids
                        .iter()
                        .map(|a| format!("{}  ({}){}", a.name, a.identifier, if a.memory_limit == Some(true) { "  · has more RAM" } else { "" }).into())
                        .collect();
                    // LiveContainer first: the usual reason to come here
                    let lc = ids.iter().position(|a| a.identifier.to_lowercase().contains("livecontainer")).unwrap_or(0) as i32;
                    let n = ids.len();
                    *app.app_ids.lock().unwrap() = ids;
                    on_ui(&app.ui, move |w| {
                        w.set_app_id_labels(model(labels));
                        w.set_app_id_index(lc);
                    });
                    app.device_progress(false, format!("{n} App IDs"), 0.0);
                }
                Err(e) => {
                    tracing::error!("listing App IDs failed: {e:#}");
                    app.device_progress(false, format!("Cannot list the App IDs: {e:#}"), 0.0);
                }
            }
        });
    }

    fn add_more_ram(self: &Arc<Self>, index: i32) {
        let Some(target) = self.app_ids.lock().unwrap().get(index.max(0) as usize).cloned() else { return };
        let app = self.clone();
        self.device_progress(true, format!("Adding the Increased Memory Limit to {}…", target.identifier), 0.0);
        self.rt.spawn(async move {
            let result = {
                let mut guard = app.session.lock().await;
                match guard.as_mut() {
                    Some(s) => s.add_memory_limit(&target.identifier).await,
                    None => Err(anyhow::anyhow!("not signed in")),
                }
            };
            match result {
                Ok(()) => app.device_progress(
                    false,
                    format!("Increased Memory Limit added to {}. Now reinstall or refresh that app (in its sideloading tool, or Install an IPA… here) so its new profile carries it.", target.name),
                    1.0,
                ),
                Err(e) => {
                    tracing::error!("adding the Increased Memory Limit failed: {e:#}");
                    app.device_progress(false, format!("Failed: {e:#}"), 0.0);
                }
            }
        });
    }

    fn stop_share(&self) {
        *self.share.lock().unwrap() = None;
        on_ui(&self.ui, |w| {
            w.set_sharing(false);
            w.set_share_url("".into());
        });
    }

    /// Copies the game's IPA and the account's signing certificate (read-only export) into the Downloads folder, for LiveContainer
    fn export(self: &Arc<Self>, id: u64) {
        let ipa = self.store.ipa(id);
        let name = self.games.lock().unwrap().iter().find(|g| g.id == id).map(|g| g.name.clone()).unwrap_or_else(|| format!("{id:016X}"));
        let dir = export::downloads_dir();
        let app = self.clone();
        on_ui(&self.ui, |w| {
            w.set_installing(true);
            w.set_install_label("Exporting…".into());
            w.set_install_fraction(0.0);
        });
        self.rt.spawn(async move {
            let result = async {
                let ui = app.ui.clone();
                let base = export::file_name(&name);
                let (ipa2, dir2) = (ipa.clone(), dir.clone());
                let out = tokio::task::spawn_blocking(move || {
                    export::copy_into(&ipa2, &dir2, &format!("{base}.ipa"), &|d, t| {
                        let f = d as f32 / t.max(1) as f32;
                        on_ui(&ui, move |w| {
                            w.set_install_fraction(f.min(0.99));
                            w.set_install_label(format!("Copying the IPA… {:.0}%", f * 100.0).into());
                        })
                    })
                })
                .await??;
                let mut guard = app.session.lock().await;
                let session = guard.as_mut().ok_or_else(|| anyhow::anyhow!("not signed in"))?;
                let (p12, whose) = session.existing_certificate_p12(&app.store, "").await?;
                let cert = export::write_certificate(&dir, &format!("{}.p12", export::file_name(session.email())), &p12)?;
                anyhow::Ok((out, cert, whose))
            }
            .await;
            let label = match result {
                Ok((ipa, cert, whose)) => format!(
                    "Exported to {}: {} and {} ({whose}'s signing key, no password). Move both to the device, import the certificate in LiveContainer, then install the IPA there.",
                    dir.display(),
                    ipa.file_name().unwrap_or_default().to_string_lossy(),
                    cert.file_name().unwrap_or_default().to_string_lossy()
                ),
                Err(e) => {
                    tracing::error!("export failed: {e:#}");
                    format!("Export failed: {e:#}")
                }
            };
            on_ui(&app.ui, move |w| {
                w.set_installing(false);
                w.set_install_label(label.into());
                w.set_install_fraction(1.0);
            });
        });
    }
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
        games: Mutex::new(vec![]),
        builds: Mutex::new(HashMap::new()),
        running: Mutex::new(None),
        session: tokio::sync::Mutex::new(None),
        devices: Mutex::new(vec![]),
        tfa: Mutex::new(None),
        certs: Mutex::new(None),
        dropped: Mutex::new(vec![]),
        share: Mutex::new(None),
        metal_version: Mutex::new(None),
        app_ids: Mutex::new(vec![]),
    });
    if let Some(id) = &settings.apple_id {
        ui.set_apple_id(id.clone().into());
        ui.set_has_saved_password(apple::credentials::load(id).is_some());
    }
    ui.set_memory_limit(settings.increased_memory_limit);
    ui.set_downloads_path(export::downloads_dir().display().to_string().into());

    // files dropped on the window (several arrive as one event each: gathered, then imported together)
    {
        use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
        let app = app.clone();
        ui.window().on_winit_window_event(move |w, ev| {
            match ev {
                WindowEvent::HoveredFile(_) => app.ui.upgrade().map(|u| u.set_dropping(true)).unwrap_or(()),
                WindowEvent::HoveredFileCancelled => app.ui.upgrade().map(|u| u.set_dropping(false)).unwrap_or(()),
                WindowEvent::DroppedFile(p) => {
                    app.dropped.lock().unwrap().push(p.clone());
                    if let Some(u) = app.ui.upgrade() {
                        u.set_dropping(false);
                    }
                    let app = app.clone();
                    slint::Timer::single_shot(std::time::Duration::from_millis(200), move || {
                        let files = std::mem::take(&mut *app.dropped.lock().unwrap());
                        if !files.is_empty() {
                            let target = app.ui.upgrade().and_then(|u| app.selected_game(&u)).map(|g| g.id);
                            app.import(files, target);
                        }
                    });
                }
                _ => {}
            }
            let _ = w;
            EventResult::Propagate
        });
    }

    let a = app.clone();
    ui.on_select(move |i| {
        if let Some(w) = a.ui.upgrade() {
            w.set_selected(i);
            a.show_selected(&w);
        }
        if i == -2 {
            a.refresh_devices();
        }
    });
    let a = app.clone();
    ui.on_add_files(move || {
        let target = a.ui.upgrade().and_then(|u| a.selected_game(&u)).map(|g| g.id);
        let a2 = a.clone();
        pick(&a, "Game files and keys", &[("Switch games and keys", &["nsp", "xci", "keys"])], true, move |files| a2.import(files, target));
    });
    let a = app.clone();
    ui.on_remove_game(move || {
        if let Some(g) = a.ui.upgrade().and_then(|u| a.selected_game(&u)) {
            match games::remove(&a.store, g.id) {
                Ok(()) => a.status(format!("Removed {}", g.name)),
                Err(e) => a.status(format!("Remove failed: {e:#}")),
            }
            if let Some(w) = a.ui.upgrade() {
                w.set_selected(-1);
            }
            a.refresh_games(None);
        }
    });
    let a = app.clone();
    ui.on_pick_xip(move || {
        let a2 = a.clone();
        pick(&a, "Xcode .xip", &[("Xcode archive", &["xip"])], false, move |f| {
            let app = a2.clone();
            std::thread::spawn(move || {
                on_ui(&app.ui, |w| {
                    w.set_tool_busy(true);
                    w.set_tool_label("Extracting the iPhone SDK from Xcode…".into());
                    w.set_tool_fraction(0.0);
                });
                let ui = app.ui.clone();
                let r = xip::extract_ios_sdk(&f[0], &app.store.sdk_dir(), &move |d, t| {
                    let frac = d as f32 / t.max(1) as f32;
                    on_ui(&ui, move |w| w.set_tool_fraction(frac));
                });
                let label = match r {
                    Ok(p) => format!("Installed {}", p.file_name().unwrap_or_default().to_string_lossy()),
                    Err(e) => format!("Failed: {e:#}"),
                };
                let app2 = app.clone();
                on_ui(&app.ui, move |w| {
                    w.set_tool_busy(false);
                    w.set_tool_label(label.into());
                    app2.show_tools(w);
                });
            });
        });
    });
    let a = app.clone();
    ui.on_pick_metal(move || {
        let a2 = a.clone();
        pick(&a, "Metal Developer Tools for Windows installer", &[("Installer", &["exe"])], false, move |f| {
            let app = a2.clone();
            std::thread::spawn(move || {
                on_ui(&app.ui, |w| {
                    w.set_tool_busy(true);
                    w.set_tool_label("Installing the Metal compiler…".into());
                    w.set_tool_fraction(0.0);
                });
                let label = match metal::install(&app.store, &f[0]) {
                    Ok(v) => {
                        *app.metal_version.lock().unwrap() = Some(v.clone());
                        format!("Installed: {v}")
                    }
                    Err(e) => format!("Failed: {e:#}"),
                };
                let app2 = app.clone();
                on_ui(&app.ui, move |w| {
                    w.set_tool_busy(false);
                    w.set_tool_label(label.into());
                    app2.show_tools(w);
                });
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
                let a3 = a2.clone();
                on_ui(&a2.ui, move |w| a3.show_tools(w));
                a2.refresh_games(None);
            }
        });
    });
    let a = app.clone();
    ui.on_build(move || {
        if let Some(g) = a.ui.upgrade().and_then(|u| a.selected_game(&u)) {
            a.build(g.id);
        }
    });
    let a = app.clone();
    ui.on_cancel_build(move || {
        if let Some((_, c)) = a.running.lock().unwrap().as_ref() {
            c.cancel();
        }
    });
    let a = app.clone();
    ui.on_sign_in(move || {
        if let Some(w) = a.ui.upgrade() {
            a.sign_in(w.get_apple_id().to_string(), w.get_password().to_string(), w.get_remember_password());
        }
    });
    let a = app.clone();
    ui.on_forget_password(move || {
        if let Some(w) = a.ui.upgrade() {
            apple::credentials::forget(&w.get_apple_id());
            w.set_has_saved_password(false);
            a.status("Saved password forgotten");
        }
    });
    let a = app.clone();
    ui.on_stop_share(move || a.stop_share());
    let a = app.clone();
    ui.on_load_app_ids(move || a.load_app_ids());
    let a = app.clone();
    ui.on_add_more_ram(move || {
        if let Some(w) = a.ui.upgrade() {
            a.add_more_ram(w.get_app_id_index());
        }
    });
    let a = app.clone();
    ui.on_save_downloads(move || {
        if let Some(g) = a.ui.upgrade().and_then(|u| a.selected_game(&u)) {
            a.export(g.id);
        }
    });
    let a = app.clone();
    ui.on_install_other_ipa(move || {
        let index = a.ui.upgrade().map(|w| w.get_device_index()).unwrap_or(0);
        let a2 = a.clone();
        pick(&a, "IPA to install", &[("iOS app", &["ipa"])], false, move |f| a2.install_other(index, f[0].clone()));
    });
    let a = app.clone();
    ui.on_sign_out(move || {
        let a2 = a.clone();
        a.rt.spawn(async move {
            *a2.session.lock().await = None;
            on_ui(&a2.ui, |w| w.set_signed_in_as("".into()));
        });
    });
    let a = app.clone();
    ui.on_refresh_devices(move || a.refresh_devices());
    let a = app.clone();
    ui.on_install(move || {
        if let Some(w) = a.ui.upgrade() {
            if let Some(g) = a.selected_game(&w) {
                let mem = w.get_memory_limit();
                {
                    let mut s = a.settings.lock().unwrap();
                    s.increased_memory_limit = mem;
                    let _ = s.save();
                }
                a.install(g.id, w.get_device_index().max(0) as usize, mem, w.get_install_target() == 1);
            }
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

    // open on the first game when the Apple tools are ready, else on the Apple tools page
    let ready = pipeline::check(&settings, &app.store).iter().all(|r| r.ok);
    let first = games::list(&app.store, pipeline::repo(&settings, &app.store).as_deref()).ok().and_then(|g| g.first().map(|g| g.id));
    app.refresh_games(if ready { first } else { None });
    app.refresh_devices();
    // signed in again with the password saved in the keyring (Apple may still ask for a code, in the dialog)
    if let Some(id) = settings.apple_id.clone().filter(|id| apple::credentials::load(id).is_some()) {
        app.sign_in(id, String::new(), true);
    }
    ui.run()?;
    drop(rt);
    Ok(())
}
