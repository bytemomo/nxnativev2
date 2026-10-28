//! Runs `dotnet <folder>/<this file's name>.dll <arguments>`: copied as dmsh_xlate.exe beside the shader translator's dll, so the nxnative scripts,
//! which run it by name, find an executable on Windows (where they cannot run the macOS launcher or a shell script). `dotnet` comes from DOTNET_ROOT, else PATH.
//! `<folder>/run/<name>.dll` goes first when it exists.
use std::path::PathBuf;
use std::process::{Command, exit};

fn main() {
    let me = std::env::current_exe().unwrap_or_else(|e| {
        eprintln!("dotnet-dll-launcher: {e}");
        exit(1)
    });
    // run/<name>.dll when there is one: the dll marked for this machine (build_ipa_from_nsp.py il_for_host), the one beside it being what the chain fingerprints
    let dll = me.with_extension("dll");
    let dll = match (me.parent(), dll.file_name()) {
        (Some(dir), Some(name)) if dir.join("run").join(name).is_file() => dir.join("run").join(name),
        _ => dll,
    };
    let dotnet = std::env::var_os("DOTNET_ROOT")
        .map(|r| PathBuf::from(r).join(if cfg!(windows) { "dotnet.exe" } else { "dotnet" }))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("dotnet"));
    let status = Command::new(dotnet).arg(&dll).args(std::env::args_os().skip(1)).status().unwrap_or_else(|e| {
        eprintln!("dotnet-dll-launcher: start dotnet for {}: {e}", dll.display());
        exit(1)
    });
    exit(status.code().unwrap_or(1));
}
