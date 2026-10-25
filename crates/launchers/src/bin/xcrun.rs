//! `xcrun -sdk <macosx|iphoneos> <metal|metallib> …` for Windows: what the nxnative shader chain asks of xcrun, run by Apple's Metal Developer Tools
//! for Windows (natively; dm/linux/bin/xcrun is the same under wine). The tools' folder is DM_METAL_WIN; the compiler is metal/<version>/bin/metal.exe
//! (bin/metal.exe only launches it), metallib.exe beside it. The SDK is given as a target triple: -mios-version-min=X → -target air64-apple-iosX.
//! In the app's bundle it runs nxnative's xcrun script (dm/linux/bin/xcrun, native on Windows) with the bundled Python: one compile per shader for the Mac and
//! iOS requests, failures remembered, and the device's compile when a game builds with it (DM_XCRUN_DEFER). Without them it runs the compiler itself.
use std::path::{Path, PathBuf};
use std::process::{Command, exit};

const MAC_MIN: &str = "15.0";

fn die(msg: &str) -> ! {
    eprintln!("xcrun (nxnativev2): {msg}");
    exit(1)
}

fn exe(root: &Path, sdk: &str, tool: &str) -> PathBuf {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root.join("metal")).map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path().join("bin")).collect()).unwrap_or_default();
    dirs.sort();
    let inner = dirs.pop();
    let platform = root.join(if sdk == "iphoneos" { "ios" } else { "macos" }).join("bin");
    for d in inner.into_iter().chain([platform, root.join("bin")]) {
        let p = d.join(format!("{tool}.exe"));
        if p.is_file() {
            return p;
        }
    }
    die(&format!("no {tool}.exe for {sdk} under {}", root.display()))
}

/// The bundle's Python and nxnative's xcrun script, beside this launcher (<bundle>/bin/xcrun.exe)
fn script() -> Option<(PathBuf, PathBuf)> {
    let bundle = std::env::current_exe().ok()?.parent()?.parent()?.to_path_buf();
    let python = ["python/python.exe", "python/bin/python3.exe", "python/bin/python.exe"].iter().map(|p| bundle.join(p)).find(|p| p.is_file())?;
    let xcrun = bundle.join("nxnative/dm/linux/bin/xcrun");
    xcrun.is_file().then_some((python, xcrun))
}

fn main() {
    if std::env::var_os("NXNATIVE_XCRUN_DIRECT").is_none() {
        if let Some((python, xcrun)) = script() {
            let status = Command::new(python).arg(xcrun).args(std::env::args().skip(1)).status().unwrap_or_else(|e| die(&format!("start the xcrun script: {e}")));
            exit(status.code().unwrap_or(1));
        }
    }
    let mut a: Vec<String> = std::env::args().skip(1).collect();
    let mut sdk = "macosx".to_string();
    if a.first().map(String::as_str) == Some("-sdk") && a.len() > 1 {
        sdk = a[1].clone();
        a.drain(..2);
    }
    if sdk != "macosx" && sdk != "iphoneos" {
        die(&format!("unsupported SDK {sdk}"));
    }
    let tool = match a.first().map(String::as_str) {
        Some(t @ ("metal" | "metallib")) => t.to_string(),
        other => die(&format!("only metal and metallib are supported, not {}", other.unwrap_or(""))),
    };
    let root = PathBuf::from(std::env::var_os("DM_METAL_WIN").unwrap_or_else(|| die("DM_METAL_WIN is not set (the Metal Developer Tools folder)")));
    let mut args: Vec<String> = a[1..].to_vec();
    let mut out: Vec<String> = vec![];
    if tool == "metal" && !args.iter().any(|x| x == "--version") {
        let min = args.iter().find_map(|x| x.strip_prefix("-mios-version-min=").map(str::to_string));
        args.retain(|x| !x.starts_with("-mios-version-min="));
        out.push("-target".into());
        out.push(if sdk == "iphoneos" { format!("air64-apple-ios{}", min.as_deref().unwrap_or("18.0")) } else { format!("air64-apple-macos{MAC_MIN}") });
    }
    out.extend(args);
    let status = Command::new(exe(&root, &sdk, &tool)).args(&out).status().unwrap_or_else(|e| die(&format!("start {tool}: {e}")));
    exit(status.code().unwrap_or(1));
}
