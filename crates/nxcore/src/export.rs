//! Exporting for LiveContainer (or any other tool): the game's IPA and the account's signing certificate (.p12) copied into the Downloads
//! folder, from where the user moves them to the device. LiveContainer then installs the IPA and signs it with the imported certificate.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The user's Downloads folder (Linux XDG_DOWNLOAD_DIR, Windows Downloads), else ~/Downloads
pub fn downloads_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|u| u.download_dir().map(Path::to_path_buf).or_else(|| Some(u.home_dir().join("Downloads"))))
        .unwrap_or_else(|| PathBuf::from("Downloads"))
}

/// A file name from a game name: "Hollow Knight: Silksong" → "Hollow Knight Silksong"
pub fn file_name(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_alphanumeric() || " -_.()".contains(c) { c } else { ' ' }).collect();
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Copies `src` to `dir/name` (a hard link when on the same disk); `progress(done, total)`
pub fn copy_into(src: &Path, dir: &Path, name: &str, progress: &dyn Fn(u64, u64)) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let dst = dir.join(name);
    if dst.exists() {
        fs::remove_file(&dst)?;
    }
    let total = fs::metadata(src)?.len();
    if fs::hard_link(src, &dst).is_ok() {
        progress(total, total);
        return Ok(dst);
    }
    let tmp = dst.with_extension("partial");
    {
        use std::io::{Read, Write};
        let mut r = fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
        let mut w = fs::File::create(&tmp)?;
        let mut buf = vec![0u8; 8 << 20];
        let mut done = 0u64;
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            w.write_all(&buf[..n])?;
            done += n as u64;
            progress(done, total);
        }
        w.sync_all()?;
    }
    fs::rename(&tmp, &dst)?;
    tracing::info!(to = %dst.display(), "export: copied");
    Ok(dst)
}

/// Writes the certificate archive next to the IPA (only readable by the user)
pub fn write_certificate(dir: &Path, name: &str, p12: &[u8]) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let dst = dir.join(name);
    fs::write(&dst, p12)?;
    tracing::info!(to = %dst.display(), "export: certificate written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dst, fs::Permissions::from_mode(0o600))?;
    }
    Ok(dst)
}
