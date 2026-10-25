//! A game's diagnostics from the device: the nxnative host's log (`Documents/log.txt` of the game's data, inside LiveContainer or the
//! standalone app's container) and the newest crash and memory (Jetsam) reports iOS keeps, copied into a folder of this computer.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use idevice::{
    IdeviceService,
    afc::{AfcClient, opcode::AfcFopenMode},
    crashreportcopymobile::{CrashReportCopyMobileClient, flush_reports},
    house_arrest::HouseArrestClient,
};

use crate::apple::{provider, report};

#[derive(Debug, Default)]
pub struct Collected {
    pub folder: PathBuf,
    pub files: Vec<String>,
    /// the last lines of the host's log, when there is one
    pub log_tail: Vec<String>,
}

async fn read(afc: &mut AfcClient, path: &str) -> Option<Vec<u8>> {
    let mut f = afc.open(path.to_string(), AfcFopenMode::RdOnly).await.ok()?;
    let b = f.read_entire().await.ok();
    let _ = f.close().await;
    b
}

/// The data folder LiveContainer gave an app (its LCAppInfo.plist names it), under LiveContainer's Documents
async fn lc_data_dir(afc: &mut AfcClient, app_bundle: &str) -> Option<String> {
    let info = read(afc, &format!("/Documents/Applications/{app_bundle}.app/LCAppInfo.plist")).await?;
    let v: plist::Value = plist::from_bytes(&info).ok()?;
    let uuid = v.as_dictionary()?.get("LCDataUUID")?.as_string()?.to_string();
    Some(format!("/Documents/Data/Application/{uuid}"))
}

/// Copies what the device has about a game into `out`: from LiveContainer `lc` (its bundle ID) when the game runs there, else from the
/// standalone app `standalone` (its bundle ID on the device); then, with `reports`, the newest crash and Jetsam reports
pub async fn collect(udid: &str, game_bundle: &str, lc: Option<&str>, standalone: Option<&str>, out: &Path, reports: bool) -> Result<Collected> {
    crate::logging::tls();
    std::fs::create_dir_all(out)?;
    let p = provider(udid).await?;
    let mut got = Collected { folder: out.to_path_buf(), ..Default::default() };
    let save = |name: &str, data: &[u8], got: &mut Collected| -> Result<()> {
        std::fs::write(out.join(name), data)?;
        got.files.push(name.to_string());
        Ok(())
    };

    // the host's log
    let mut log = None;
    if let Some(lc) = lc {
        if let Ok(mut afc) = HouseArrestClient::connect(&p).await.map_err(report)?.vend_documents(lc.to_string()).await {
            if let Some(dir) = lc_data_dir(&mut afc, game_bundle).await {
                for name in ["log.txt", "log.prev.txt"] {
                    if let Some(b) = read(&mut afc, &format!("{dir}/Documents/{name}")).await {
                        save(name, &b, &mut got)?;
                        if name == "log.txt" {
                            log = Some(b);
                        }
                    }
                }
                if let Ok(list) = afc.list_dir(format!("{dir}/Documents")).await {
                    std::fs::write(out.join("documents.txt"), list.join("\n"))?;
                }
            }
        }
    }
    if log.is_none() {
        if let Some(app) = standalone {
            if let Ok(mut afc) = HouseArrestClient::connect(&p).await.map_err(report)?.vend_container(app.to_string()).await {
                for name in ["log.txt", "log.prev.txt"] {
                    if let Some(b) = read(&mut afc, &format!("/Documents/{name}")).await {
                        save(name, &b, &mut got)?;
                        if name == "log.txt" {
                            log = Some(b);
                        }
                    }
                }
            }
        }
    }
    if let Some(b) = &log {
        let t = String::from_utf8_lossy(b);
        let lines: Vec<&str> = t.lines().collect();
        got.log_tail = lines[lines.len().saturating_sub(40)..].iter().map(|s| s.to_string()).collect();
    }

    if reports {
        let r = crash_reports(udid, out).await?;
        got.files.extend(r);
    }
    tracing::info!(folder = %out.display(), files = got.files.len(), "diagnostics collected");
    Ok(got)
}

/// Every game's log on the device (inside LiveContainer and standalone: the apps whose bundle ID starts with org.nxnative.), each in a folder
/// named after it (`names`: bundle ID → name), and the newest crash and Jetsam reports once, under `out`
pub async fn collect_all(udid: &str, lc: Option<&str>, names: &[(String, String)], out: &Path) -> Result<Vec<String>> {
    crate::logging::tls();
    std::fs::create_dir_all(out)?;
    let mut bundles: Vec<(String, Option<String>)> = vec![];   // (game bundle ID, its standalone app on the device)
    if let Some(lc) = lc {
        for (b, _) in crate::lc_usb::contents(udid, lc).await.unwrap_or_default() {
            if b.starts_with("org.nxnative.") {
                bundles.push((b, None));
            }
        }
    }
    if let Ok(info) = crate::device::info(udid).await {
        for a in info.apps.iter().filter(|a| a.bundle_id.starts_with("org.nxnative.")) {
            let game = a.bundle_id.split('.').take(3).collect::<Vec<_>>().join(".");
            match bundles.iter_mut().find(|(b, _)| *b == game) {
                Some(e) => e.1 = Some(a.bundle_id.clone()),
                None => bundles.push((game, Some(a.bundle_id.clone()))),
            }
        }
    }
    let mut done = vec![];
    for (game, standalone) in &bundles {
        let name = names.iter().find(|(b, _)| b == game).map(|(_, n)| crate::export::file_name(n)).unwrap_or_else(|| game.clone());
        let c = collect(udid, game, lc, standalone.as_deref(), &out.join(&name), false).await?;
        if !c.files.is_empty() {
            done.push(name);
        }
    }
    let reports = crash_reports(udid, &out.join("crash reports")).await?;
    if !reports.is_empty() {
        done.push(format!("{} crash reports", reports.len()));
    }
    tracing::info!(folder = %out.display(), games = bundles.len(), "every game's logs collected");
    Ok(done)
}

/// The newest crash and memory reports iOS keeps: LiveContainer's, the games' (DmIOS) and Jetsam's
async fn crash_reports(udid: &str, out: &Path) -> Result<Vec<String>> {
    std::fs::create_dir_all(out)?;
    let p = provider(udid).await?;
    let mut files = vec![];
    let _ = flush_reports(&p).await;
    let mut cr = CrashReportCopyMobileClient::connect(&p).await.map_err(report).context("open the crash reports")?;
    let mut names = cr.ls(None).await.map_err(report)?;
    names.sort();
    let take = |pat: &str, n: usize| names.iter().rev().filter(|f| f.contains(pat)).take(n).cloned().collect::<Vec<_>>();
    let mut wanted = take("LiveContainer", 3);
    wanted.extend(take("DmIOS", 3));
    wanted.extend(take("JetsamEvent", 2));
    for name in wanted {
        if let Ok(b) = cr.pull(name.clone()).await {
            let f = name.replace('/', "_");
            std::fs::write(out.join(&f), &b)?;
            files.push(f);
        }
    }
    Ok(files)
}
