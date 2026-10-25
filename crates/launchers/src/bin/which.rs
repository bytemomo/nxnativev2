//! `which <program>…` for Windows: the full path of each program found in PATH (with PATHEXT's extensions), as the Unix tool prints it.
//! The nxnative shader chain fingerprints spirv-cross through `which spirv-cross` (dm/gfx/tools/dmsh_chain.py), and that file is part of the
//! chain version, so Windows gets the command rather than the file a change.
use std::path::PathBuf;
use std::process::exit;

fn main() {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let exts: Vec<String> = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into()).split(';').filter(|e| !e.is_empty()).map(|e| e.to_lowercase()).collect();
    let mut missing = false;
    for name in std::env::args().skip(1) {
        let mut candidates: Vec<String> = vec![name.clone()];
        if PathBuf::from(&name).extension().is_none() {
            candidates.extend(exts.iter().map(|e| format!("{name}{e}")));
        }
        let found = std::env::split_paths(&path).find_map(|d| candidates.iter().map(|c| d.join(c)).find(|p| p.is_file()));
        match found {
            Some(p) => println!("{}", p.display()),
            None => missing = true,
        }
    }
    exit(if missing { 1 } else { 0 });
}
