//! Command-line front end of nxcore: the app's steps, one command each (for testing and scripting).
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use futures::FutureExt;
use nxcore::{apple, games, metal, pipeline, settings::Settings, store::Store, xip};

#[derive(Parser)]
#[command(name = "nxnative", about = "Switch games to iPhone / iPad apps (nxnative)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show or change settings (data folder, nxnative checkout, python)
    Config {
        #[arg(long)]
        data_dir: Option<PathBuf>,
        #[arg(long)]
        repo: Option<PathBuf>,
        #[arg(long)]
        python: Option<PathBuf>,
    },
    /// What the build needs, and what is missing
    Check,
    /// Unpack the build tools a packaged app carries (done by the first build too)
    Setup,
    /// Extract the iPhoneOS SDK from an Xcode .xip into the data folder (or into DEST)
    Sdk { xip: PathBuf, dest: Option<PathBuf> },
    /// Install Apple's Metal Developer Tools for Windows from their installer
    Metal { installer: PathBuf },
    /// Import game files (.nsp / .xci) and keys (.keys)
    Import {
        files: Vec<PathBuf>,
        /// the game keys go to when no game files are given (title ID)
        #[arg(long)]
        game: Option<String>,
    },
    /// List the games
    Games,
    /// Build a game's IPA
    Build { game: String },
    /// List connected iPhones / iPads
    Devices,
    /// Sign in to Apple and install a game's IPA on a device
    Install {
        game: String,
        #[arg(long)]
        apple_id: Option<String>,
        /// the device's UDID (default: the only connected device)
        #[arg(long)]
        device: Option<String>,
    },
    /// Copy a game's IPA and the account's signing certificate (.p12, empty password) into a folder (default: Downloads), for LiveContainer
    Export {
        game: String,
        #[arg(long)]
        to: Option<PathBuf>,
        #[arg(long)]
        apple_id: Option<String>,
    },
    /// Sign in to Apple and install any IPA (LiveContainer, for example) on a device
    InstallIpa {
        ipa: PathBuf,
        #[arg(long)]
        apple_id: Option<String>,
        #[arg(long)]
        device: Option<String>,
    },
    /// Share a game with LiveContainer over Wi-Fi: a page for the device (certificate import + install links) until Ctrl+C
    Serve {
        game: String,
        /// sign in to add the account's certificate to the page
        #[arg(long)]
        apple_id: Option<String>,
        /// no certificate (no sign-in): only the IPA
        #[arg(long)]
        no_certificate: bool,
        #[arg(long, default_value_t = nxcore::serve::DEFAULT_PORT)]
        port: u16,
    },
    /// List the account's App IDs (the apps installed with it)
    AppIds {
        #[arg(long)]
        apple_id: Option<String>,
    },
    /// Add the Increased Memory Limit to an App ID (e.g. LiveContainer's); then reinstall or refresh that app
    MoreRam {
        /// the App ID's bundle ID, or a part of it (e.g. livecontainer)
        app: String,
        #[arg(long)]
        apple_id: Option<String>,
    },
    /// Forget the Apple ID password saved in the system keyring
    Forget { apple_id: String },
}

fn parse_id(s: &str) -> Result<u64> {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).with_context(|| format!("not a title ID: {s}"))
}

fn bar(label: &str, done: u64, total: u64) {
    eprint!("\r{label} {}%   ", done * 100 / total.max(1));
}

fn prompt(q: &str) -> String {
    eprint!("{q}");
    std::io::stderr().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).ok();
    s.trim().to_string()
}

async fn pick_device(device: Option<String>) -> Result<String> {
    let devs = apple::devices().await?;
    Ok(match device {
        Some(d) => d,
        None if devs.len() == 1 => devs[0].udid.clone(),
        None => bail!("{} devices connected: pick one with --device", devs.len()),
    })
}

/// Signs in with the saved password (system keyring) or asks for it, and saves it after a successful sign-in
async fn sign_in(store: &Store, settings: &mut Settings, apple_id: Option<String>) -> Result<apple::Session> {
    let apple_id = apple_id.or(settings.apple_id.clone()).context("no Apple ID: give --apple-id")?;
    let saved = apple::credentials::load(&apple_id);
    let password = match &saved {
        Some(p) => {
            eprintln!("using the password saved in the system keyring for {apple_id}");
            p.clone()
        }
        None => prompt("Apple ID password: "),
    };
    let ask: apple::TwoFactorAsk = Arc::new(|p: apple::TwoFactorPrompt| {
        async move {
            if let Some(e) = &p.last_error {
                eprintln!("Apple: {e}");
            }
            for (id, n) in &p.numbers {
                eprintln!("  trusted number {id}: {n}");
            }
            let a = prompt("2FA code (or: sms <number ID>, devices, resend, abort): ");
            match a.split_whitespace().collect::<Vec<_>>().as_slice() {
                ["sms", id] => id.parse().map(apple::TwoFactorAnswer::SmsTo).unwrap_or(apple::TwoFactorAnswer::Abort),
                ["devices"] => apple::TwoFactorAnswer::ToDevices,
                ["resend"] => apple::TwoFactorAnswer::Resend,
                ["abort"] | [] => apple::TwoFactorAnswer::Abort,
                _ => apple::TwoFactorAnswer::Code(a),
            }
        }
        .boxed()
    });
    let certs: apple::CertsAsk = Arc::new(|list: Vec<(String, String)>| {
        async move {
            eprintln!("The account has the most development certificates allowed:");
            for (name, serial) in &list {
                eprintln!("  {name}  {serial}");
            }
            let s = prompt("serial to revoke (apps signed with it stop opening; empty = stop): ");
            if s.is_empty() { vec![] } else { vec![s] }
        }
        .boxed()
    });
    let session = apple::login(store, settings, &apple_id, &password, ask, certs).await;
    let session = match session {
        Err(e) if saved.is_some() => {
            apple::credentials::forget(&apple_id);
            return Err(e.context("sign-in with the saved password failed (it is forgotten now; run again to type it)"));
        }
        s => s?,
    };
    if saved.is_none() && apple::credentials::save(&apple_id, &password).is_ok() {
        eprintln!("password saved in the system keyring");
    }
    settings.apple_id = Some(session.email().to_string());
    settings.save()?;
    Ok(session)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut settings = Settings::load()?;
    let store = Store::new(settings.data_dir()?)?;
    nxcore::logging::init(&store);
    let repo = pipeline::repo(&settings, &store);
    match Cli::parse().cmd {
        Cmd::Config { data_dir, repo, python } => {
            if data_dir.is_some() || repo.is_some() || python.is_some() {
                settings.data_dir = data_dir.or(settings.data_dir);
                settings.nxnative_repo = repo.or(settings.nxnative_repo);
                settings.python = python.or(settings.python);
                settings.save()?;
            }
            println!("{}", toml::to_string_pretty(&settings)?);
            println!("# data folder: {}", settings.data_dir()?.display());
        }
        Cmd::Check => {
            for r in pipeline::check(&settings, &store) {
                println!("{} {:<40} {}", if r.ok { "ok " } else { "-- " }, r.name, r.detail);
            }
            let k = apple::credentials::available();
            println!("{} {:<40} {}", if k { "ok " } else { "-- " }, "system keyring", if k { "Apple ID password and sign-in state are kept there" } else { "not available: sign-in state is kept in the data folder" });
        }
        Cmd::Forget { apple_id } => {
            apple::credentials::forget(&apple_id);
            println!("forgot the saved password of {apple_id}");
        }
        Cmd::Setup => {
            pipeline::prepare(&settings, &store, &|d, t, step| {
                if t > 0 { eprint!("\r{step} {}%   ", d * 100 / t) } else { eprint!("\r{step}…                    ") }
            })
            .await?;
            eprintln!();
            match nxcore::bundle::ready(&store) {
                Some(d) => println!("bundle:   {}", d.display()),
                None => println!("bundle:   none (a development build: set nxnative_repo / python with `nxnative config`)"),
            }
            println!("nxnative: {}", pipeline::repo(&settings, &store).map_or("-".into(), |p| p.display().to_string()));
            println!("python:   {}", pipeline::python(&settings, &store).display());
        }
        Cmd::Sdk { xip, dest } => {
            let dest = dest.unwrap_or_else(|| store.sdk_dir());
            let t = std::time::Instant::now();
            let sdk = xip::extract_ios_sdk(&xip, &dest, &|d, t| bar("extracting", d, t))?;
            eprintln!("\r{} ({:.0} s)", sdk.display(), t.elapsed().as_secs_f64());
        }
        Cmd::Metal { installer } => println!("{}", metal::install(&store, &installer)?),
        Cmd::Import { files, game } => {
            let target = game.as_deref().map(parse_id).transpose()?;
            let r = games::import(&store, &files, target, &|d, t| bar("importing", d, t))?;
            eprintln!();
            for p in &r.imported {
                println!("imported {}", p.display());
            }
            for (p, why) in &r.skipped {
                println!("skipped  {}: {why}", p.display());
            }
        }
        Cmd::Games => {
            for g in games::list(&store, repo.as_deref())? {
                println!(
                    "{:016X}  {}{}  base {}, update {}, {} DLC, keys {}, IPA {}",
                    g.id,
                    g.name,
                    if g.supported { "" } else { " (no profile: provisional)" },
                    if g.has_base() { "yes" } else { "no" },
                    g.newest_update().map_or("none".into(), |v| format!("v{v}")),
                    g.dlc_count(),
                    if g.has_keys { "yes" } else { "no" },
                    g.ipa.as_ref().map_or("not built".into(), |p| p.display().to_string()),
                );
            }
        }
        Cmd::Build { game } => {
            let id = parse_id(&game)?;
            let g = games::game(&store, repo.as_deref(), id)?;
            if !g.missing().is_empty() {
                bail!("{} is missing {}", g.name, g.missing().join(" and "));
            }
            pipeline::prepare(&settings, &store, &|d, t, step| {
                if t > 0 { eprint!("\r{step} {}%   ", d * 100 / t) }
            })
            .await?;
            eprintln!();
            let h = pipeline::start(&settings, &store, id, &Default::default(), |e| match e {
                pipeline::Event::Stage { k, n, name } => eprintln!("\n[{k}/{n}] {name}"),
                pipeline::Event::Progress { step, frac, count, .. } => {
                    eprint!("\r  {step} {} {count}        ", frac.map_or(String::new(), |f| format!("{:.0}%", f * 100.0)))
                }
                pipeline::Event::StageEnd { name, ok, summary } => eprintln!("\r  {} {name}: {summary}", if ok { "done" } else { "FAILED" }),
                pipeline::Event::Say(m) => eprintln!("\r  {m}"),
                pipeline::Event::Log(l) => eprintln!("  | {l}"),
            })?;
            h.wait()?;
            println!("{}", store.ipa(id).display());
        }
        Cmd::Devices => {
            for d in apple::devices().await? {
                println!("{}  {} (iOS {}){}", d.udid, d.name, d.version, if d.usb { "" } else { " [network]" });
            }
        }
        Cmd::Install { game, apple_id, device } => {
            let id = parse_id(&game)?;
            let ipa = store.ipa(id);
            if !ipa.is_file() {
                bail!("no IPA for {game} yet: build it first");
            }
            let udid = pick_device(device).await?;
            let mut session = sign_in(&store, &mut settings, apple_id).await?;
            apple::install(&mut session, &udid, &ipa, settings.increased_memory_limit, Arc::new(|f| eprint!("\rinstalling {:.0}%   ", f * 100.0))).await?;
            eprintln!("\rinstalled            ");
        }
        Cmd::InstallIpa { ipa, apple_id, device } => {
            let udid = pick_device(device).await?;
            let mut session = sign_in(&store, &mut settings, apple_id).await?;
            apple::install(&mut session, &udid, &ipa, settings.increased_memory_limit, Arc::new(|f| eprint!("\rinstalling {:.0}%   ", f * 100.0))).await?;
            eprintln!("\rinstalled            ");
        }
        Cmd::Serve { game, apple_id, no_certificate, port } => {
            let id = parse_id(&game)?;
            let ipa = store.ipa(id);
            if !ipa.is_file() {
                bail!("no IPA for {game} yet: build it first");
            }
            let g = games::game(&store, repo.as_deref(), id)?;
            let certificate = if no_certificate {
                None
            } else {
                let mut session = sign_in(&store, &mut settings, apple_id).await?;
                Some(session.existing_certificate_p12(&store, "").await?.0)
            };
            let server = nxcore::serve::start(nxcore::serve::Share { name: g.name.clone(), ipa, certificate }, port)?;
            println!("Open on the device (same Wi-Fi): {}", server.url);
            println!("Serving until Ctrl+C.");
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let sent = server.sent.load(std::sync::atomic::Ordering::Relaxed);
                if sent > 0 {
                    eprint!("\rIPA sent: {:.2} GB   ", sent as f64 / 1e9);
                }
            }
        }
        Cmd::AppIds { apple_id } => {
            let mut session = sign_in(&store, &mut settings, apple_id).await?;
            for a in session.app_ids().await? {
                let m = match a.memory_limit { Some(true) => "  [more RAM]", Some(false) => "", None => "" };
                println!("{}  {}{m}", a.identifier, a.name);
            }
        }
        Cmd::MoreRam { app, apple_id } => {
            let mut session = sign_in(&store, &mut settings, apple_id).await?;
            let ids = session.app_ids().await?;
            let hits: Vec<_> = ids.iter().filter(|a| a.identifier == app || a.identifier.to_lowercase().contains(&app.to_lowercase())).collect();
            let target = match hits.as_slice() {
                [one] => one.identifier.clone(),
                [] => bail!("no App ID matches {app}: see `nxnative app-ids`"),
                many => bail!("{} App IDs match {app}: {}", many.len(), many.iter().map(|a| a.identifier.as_str()).collect::<Vec<_>>().join(", ")),
            };
            session.add_memory_limit(&target).await?;
            println!("Increased Memory Limit added to {target}. Now reinstall or refresh that app (its new provisioning profile carries it).");
        }
        Cmd::Export { game, to, apple_id } => {
            let id = parse_id(&game)?;
            let ipa = store.ipa(id);
            if !ipa.is_file() {
                bail!("no IPA for {game} yet: build it first");
            }
            let g = games::game(&store, repo.as_deref(), id)?;
            let dir = to.unwrap_or_else(nxcore::export::downloads_dir);
            let base = nxcore::export::file_name(&g.name);
            let out = nxcore::export::copy_into(&ipa, &dir, &format!("{base}.ipa"), &|d, t| bar("copying the IPA", d, t))?;
            eprintln!();
            println!("{}", out.display());
            let mut session = sign_in(&store, &mut settings, apple_id).await?;
            let (p12, whose) = session.existing_certificate_p12(&store, "").await?;
            let cert = nxcore::export::write_certificate(&dir, &format!("{}.p12", nxcore::export::file_name(session.email())), &p12)?;
            println!("{} ({whose}'s signing key, empty password)", cert.display());
            println!("On the device: LiveContainer → Settings → Import Certificate (this .p12, no password), then install the IPA in LiveContainer.");
        }
    }
    Ok(())
}
