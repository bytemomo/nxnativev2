//! `xcrun -sdk <macosx|iphoneos> <metal|metallib> …` for Windows: what the nxnative shader chain asks of xcrun, run by Apple's Metal Developer Tools
//! for Windows (natively; dm/linux/bin/xcrun is the same under wine). The tools' folder is DM_METAL_WIN; the compiler is metal/<version>/bin/metal.exe
//! (bin/metal.exe only launches it), metallib.exe beside it. The SDK is given as a target triple: -mios-version-min=X → -target air64-apple-iosX.
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

fn main() {
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
