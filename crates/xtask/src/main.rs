//! `cargo xtask bundle [--target linux|windows]`: the packaged app, one executable per platform with everything the build needs appended
//! (see nxcore::bundle). Built on Linux x86_64 for both targets (Windows: cross-compiled with mingw-w64, Windows archives and wheels):
//!
//! 1. nxnative: the revision of patches/nxnative/UPSTREAM, with patches/nxnative/common and patches/nxnative/<platform> applied
//! 2. Python: python-build-standalone with the pipeline's modules (pip; Windows wheels for Windows)
//! 3. the .NET runtime, and nx_extract published for it
//! 4. the iOS app's Swift sources compiled to one arm64 iOS object (swiftc with Xcode's SDK and Swift resources from the data folder)
//! 5. spirv-cross built from source (static runtime; mingw for Windows)
//! 6. clang and lld with clang's headers, from LLVM's release
//! 7. Windows: xcrun.exe and the dotnet dll launcher (crates/launchers): the pipeline runs these by name
//!
//! Downloads are cached in target/bundle-cache; the result goes to dist/.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

const PY_RELEASE: &str = "20261021";
const PY_VERSION: &str = "3.13.17";
const PY_MODULES: &[&str] = &["lz4", "capstone", "fonttools", "pillow", "backports.zstd"];
const DOTNET_RUNTIME: &str = "10.0.13";
const SPIRV_CROSS_TAG: &str = "vulkan-sdk-1.4.363.0";
const LLVM_VERSION: &str = "23.1.4";
const MIN_IOS: &str = "26.0";
const WIN_RUST_TARGET: &str = "x86_64-pc-windows-gnu";

struct Ctx {
    root: PathBuf,
    cache: PathBuf,
    payload: PathBuf,
    platform: &'static str,
    windows: bool,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("bundle") => bundle(&args[1..]),
        _ => bail!("usage: cargo xtask bundle [--target linux|windows] [--sdk-dir <folder with iPhoneOS*.sdk and swift-resources>] [--cli] [--keep]"),
    }
}

fn say(s: impl AsRef<str>) {
    eprintln!("[xtask] {}", s.as_ref());
}

fn run(cmd: &mut Command) -> Result<()> {
    let st = cmd.status().with_context(|| format!("start {cmd:?}"))?;
    ensure!(st.success(), "{cmd:?} failed ({st})");
    Ok(())
}

fn arg<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn bundle(args: &[String]) -> Result<()> {
    ensure!(cfg!(target_os = "linux") && cfg!(target_arch = "x86_64"), "bundles are built on Linux x86_64 (for Linux and Windows)");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
    let windows = match arg(args, "--target").unwrap_or("linux") {
        "linux" => false,
        "windows" => true,
        t => bail!("unknown target {t}: linux or windows"),
    };
    let sdk_dir = match arg(args, "--sdk-dir") {
        Some(d) => PathBuf::from(d),
        None => {
            let s = nxcore::settings::Settings::load()?;
            nxcore::store::Store::new(s.data_dir()?)?.sdk_dir()
        }
    };
    let name = if windows { "windows-x64" } else { "linux-x64" };
    let ctx = Ctx {
        cache: root.join("target/bundle-cache"),
        payload: root.join(format!("target/bundle/{name}/payload")),
        platform: if windows { "windows" } else { "linux" },
        windows,
        root: root.clone(),
    };
    if ctx.payload.exists() {
        fs::remove_dir_all(&ctx.payload)?;
    }
    let _ = fs::remove_file(ctx.payload.with_extension("tar.zst"));
    fs::create_dir_all(&ctx.payload)?;
    fs::create_dir_all(&ctx.cache)?;

    nxnative(&ctx)?;
    python(&ctx)?;
    dotnet(&ctx)?;
    swift_object(&ctx, &sdk_dir)?;
    spirv_cross(&ctx)?;
    llvm(&ctx)?;
    if windows {
        launchers(&ctx)?;
    }
    shrink(&ctx)?;

    say("building the app (profile dist: no debug info, stripped)");
    let (exe, out) = app(&ctx, "nxapp", "nxnativev2")?;
    pack(&ctx, &exe, &root.join(format!("dist/nxnativev2-{name}{out}")))?;
    if args.iter().any(|a| a == "--cli") {
        // the command-line front end with the same payload (testing, scripting)
        let (exe, out) = app(&ctx, "nxcli", "nxnative")?;
        pack(&ctx, &exe, &root.join(format!("dist/nxnative-cli-{name}{out}")))?;
    }
    if !args.iter().any(|a| a == "--keep") {
        let _ = fs::remove_dir_all(ctx.payload.parent().unwrap());
    }
    Ok(())
}

/// Builds a package of the workspace for the target; returns the executable and the file extension of the output
fn app(ctx: &Ctx, package: &str, bin: &str) -> Result<(PathBuf, &'static str)> {
    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--profile", "dist", "-p", package]).current_dir(&ctx.root);
    if ctx.windows {
        cmd.args(["--target", WIN_RUST_TARGET]);
        run(&mut cmd)?;
        return Ok((ctx.root.join(format!("target/{WIN_RUST_TARGET}/dist/{bin}.exe")), ".exe"));
    }
    run(&mut cmd)?;
    Ok((ctx.root.join(format!("target/dist/{bin}")), ""))
}

/// Downloads `url` into the cache (once) and returns its path
fn fetch(ctx: &Ctx, url: &str, name: &str) -> Result<PathBuf> {
    let p = ctx.cache.join(name);
    if p.is_file() {
        return Ok(p);
    }
    say(format!("downloading {url}"));
    let client = reqwest::blocking::Client::builder().user_agent("nxnativev2-xtask").timeout(None).build()?;
    let mut res = client.get(url).send()?.error_for_status()?;
    let tmp = p.with_extension("partial");
    let mut f = File::create(&tmp)?;
    std::io::copy(&mut res, &mut f)?;
    f.sync_all()?;
    fs::rename(&tmp, &p)?;
    Ok(p)
}

/// Unpacks a .zip into `out`, dropping `strip` leading components
fn unzip(archive: &Path, out: &Path, strip: usize) -> Result<()> {
    let mut z = zip::ZipArchive::new(File::open(archive)?)?;
    for i in 0..z.len() {
        let mut e = z.by_index(i)?;
        let Some(rel) = e.enclosed_name() else { continue };
        let rel: PathBuf = rel.components().skip(strip).collect();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dst = out.join(rel);
        if e.is_dir() {
            fs::create_dir_all(&dst)?;
        } else {
            fs::create_dir_all(dst.parent().unwrap())?;
            std::io::copy(&mut e, &mut File::create(&dst)?)?;
        }
    }
    Ok(())
}

fn python(ctx: &Ctx) -> Result<()> {
    let triple = if ctx.windows { "x86_64-pc-windows-msvc" } else { "x86_64-unknown-linux-gnu" };
    // the stripped variant: binutils' strip breaks these binaries (python3 then fails with "symbol lookup error")
    let name = format!("cpython-{PY_VERSION}+{PY_RELEASE}-{triple}-install_only_stripped.tar.gz");
    let url = format!("https://github.com/astral-sh/python-build-standalone/releases/download/{PY_RELEASE}/{name}");
    let tarball = fetch(ctx, &url, &name)?;
    let out = ctx.payload.join("python");
    say(format!("Python {PY_VERSION} ({}) with {}", ctx.platform, PY_MODULES.join(" ")));
    untar(&tarball, &out, 1, &|_| true)?;
    if ctx.windows {
        // Windows wheels, installed by this computer's pip into the Windows Python's site-packages
        run(Command::new("python3")
            .args(["-m", "pip", "install", "--disable-pip-version-check", "--no-input", "--quiet", "--no-compile", "--only-binary=:all:", "--platform", "win_amd64"])
            .args(["--python-version", "3.13", "--implementation", "cp", "--target"])
            .arg(out.join("Lib/site-packages"))
            .args(PY_MODULES))?;
    } else {
        run(Command::new(out.join("bin/python3")).args(["-m", "pip", "install", "--disable-pip-version-check", "--no-input", "--quiet", "--no-compile"]).args(PY_MODULES))?;
    }
    Ok(())
}

fn dotnet(ctx: &Ctx) -> Result<()> {
    let (rid, ext) = if ctx.windows { ("win-x64", "zip") } else { ("linux-x64", "tar.gz") };
    let name = format!("dotnet-runtime-{DOTNET_RUNTIME}-{rid}.{ext}");
    let archive = fetch(ctx, &format!("https://builds.dotnet.microsoft.com/dotnet/Runtime/{DOTNET_RUNTIME}/{name}"), &name)?;
    say(format!(".NET runtime {DOTNET_RUNTIME} ({rid})"));
    if ctx.windows { unzip(&archive, &ctx.payload.join("dotnet"), 0)? } else { untar(&archive, &ctx.payload.join("dotnet"), 0, &|_| true)? }
    // nx_extract for that runtime (framework-dependent), with the build host's .NET SDK
    say(format!("nx_extract (dotnet publish -r {rid})"));
    let out = ctx.payload.join("prebuilt/nx_extract");
    run(Command::new("dotnet")
        .args(["publish", "-c", "Release", "-r", rid, "--self-contained", "false", "-v", "quiet", "-nologo", "-o"])
        .arg(&out)
        .arg(ctx.payload.join("nxnative/dm/tools/nx_extract"))
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1"))?;
    ensure!(out.join(if ctx.windows { "nx_extract.exe" } else { "nx_extract" }).is_file(), "dotnet publish made no nx_extract");
    // the publish leaves bin/obj in the checkout
    for d in ["bin", "obj"] {
        let _ = fs::remove_dir_all(ctx.payload.join("nxnative/dm/tools/nx_extract").join(d));
    }
    Ok(())
}

fn spirv_cross(ctx: &Ctx) -> Result<()> {
    let src = ctx.cache.join(format!("SPIRV-Cross-{SPIRV_CROSS_TAG}"));
    if !src.join("CMakeLists.txt").is_file() {
        say(format!("cloning SPIRV-Cross {SPIRV_CROSS_TAG}"));
        let _ = fs::remove_dir_all(&src);
        run(Command::new("git").args(["clone", "-q", "--depth", "1", "-b", SPIRV_CROSS_TAG, "https://github.com/KhronosGroup/SPIRV-Cross.git"]).arg(&src))?;
    }
    let build = src.join(if ctx.windows { "build-mingw" } else { "build-static" });
    let bin = build.join(if ctx.windows { "spirv-cross.exe" } else { "spirv-cross" });
    if !bin.is_file() {
        say(format!("building spirv-cross ({})", ctx.platform));
        let mut cmake = Command::new("cmake");
        cmake.arg("-S").arg(&src).arg("-B").arg(&build);
        cmake.args(["-G", "Ninja", "-DCMAKE_BUILD_TYPE=Release", "-DSPIRV_CROSS_SHARED=OFF", "-DSPIRV_CROSS_STATIC=ON", "-DSPIRV_CROSS_ENABLE_TESTS=OFF", "-DSPIRV_CROSS_CLI=ON"]);
        if ctx.windows {
            cmake.args(["-DCMAKE_SYSTEM_NAME=Windows", "-DCMAKE_C_COMPILER=x86_64-w64-mingw32-gcc", "-DCMAKE_CXX_COMPILER=x86_64-w64-mingw32-g++", "-DCMAKE_EXE_LINKER_FLAGS=-static"]);
        } else {
            cmake.arg("-DCMAKE_EXE_LINKER_FLAGS=-static-libstdc++ -static-libgcc");
        }
        run(&mut cmake)?;
        run(Command::new("cmake").arg("--build").arg(&build).args(["--target", "spirv-cross"]))?;
    }
    let dst = ctx.payload.join("bin").join(bin.file_name().unwrap());
    fs::create_dir_all(dst.parent().unwrap())?;
    fs::copy(&bin, &dst)?;
    say("spirv-cross");
    Ok(())
}

fn llvm(ctx: &Ctx) -> Result<()> {
    let name = if ctx.windows { format!("clang+llvm-{LLVM_VERSION}-x86_64-pc-windows-msvc.tar.zst") } else { format!("LLVM-{LLVM_VERSION}-Linux-X64.tar.zst") };
    let tarball = fetch(ctx, &format!("https://github.com/llvm/llvm-project/releases/download/llvmorg-{LLVM_VERSION}/{name}"), &name)?;
    let major = LLVM_VERSION.split('.').next().unwrap();
    say(format!("clang and lld {LLVM_VERSION} (of LLVM's release, {})", ctx.platform));
    let out = ctx.payload.join("llvm");
    let keep_bin: Vec<String> = if ctx.windows {
        vec!["bin/clang.exe".into(), "bin/ld64.lld.exe".into()]
    } else {
        vec![format!("bin/clang-{major}"), "bin/clang".into(), "bin/lld".into(), "bin/ld64.lld".into()]
    };
    let headers = format!("lib/clang/{major}/include");
    untar(&tarball, &out, 1, &|p: &Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        keep_bin.iter().any(|k| s == *k) || s == format!("lib/clang/{major}") || s == "lib/clang" || s == "lib" || s == "bin" || s.starts_with(&headers)
    })?;
    let (clang, ld) = if ctx.windows { ("bin/clang.exe", "bin/ld64.lld.exe") } else { ("bin/clang", "bin/ld64.lld") };
    ensure!(out.join(clang).exists() && out.join(ld).exists(), "the LLVM release had no clang / ld64.lld");
    Ok(())
}

/// Windows: the executables the pipeline runs by name (crates/launchers)
fn launchers(ctx: &Ctx) -> Result<()> {
    say("launchers (xcrun.exe, dotnet-dll-launcher.exe)");
    run(Command::new("cargo").args(["build", "--profile", "dist", "-p", "launchers", "--target", WIN_RUST_TARGET]).current_dir(&ctx.root))?;
    let from = ctx.root.join(format!("target/{WIN_RUST_TARGET}/dist"));
    for exe in ["xcrun.exe", "dotnet-dll-launcher.exe"] {
        fs::copy(from.join(exe), ctx.payload.join("bin").join(exe))?;
    }
    Ok(())
}

fn untar(archive: &Path, out: &Path, strip: usize, keep: &dyn Fn(&Path) -> bool) -> Result<()> {
    let f = File::open(archive)?;
    let reader: Box<dyn Read> = if archive.to_string_lossy().ends_with(".zst") {
        // LLVM's archives use zstd's long window (up to 2 GB)
        let mut d = zstd::stream::read::Decoder::new(f)?;
        d.window_log_max(31)?;
        Box::new(d)
    } else {
        Box::new(flate2::read::GzDecoder::new(f))
    };
    let mut ar = tar::Archive::new(reader);
    ar.set_preserve_permissions(true);
    fs::create_dir_all(out)?;
    for e in ar.entries()? {
        let mut e = e?;
        if e.header().entry_type().is_pax_global_extensions() {
            continue;
        }
        let p = e.path()?.into_owned();
        // "./dotnet" and "dotnet" alike
        let rel: PathBuf = p.components().filter(|c| !matches!(c, std::path::Component::CurDir)).skip(strip).collect();
        if rel.as_os_str().is_empty() || !keep(&rel) {
            continue;
        }
        let dst = out.join(&rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        e.unpack(&dst)?;
    }
    Ok(())
}
fn upstream(ctx: &Ctx, key: &str) -> Result<String> {
    let s = fs::read_to_string(ctx.root.join("patches/nxnative/UPSTREAM"))?;
    s.lines().find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('\t'))).map(|v| v.trim().to_string()).with_context(|| format!("no {key} in UPSTREAM"))
}
fn nxnative(ctx: &Ctx) -> Result<()> {
    let (repo, rev) = (upstream(ctx, "repo")?, upstream(ctx, "rev")?);
    let tarball = fetch(ctx, &format!("{repo}/archive/{rev}.tar.gz"), &format!("nxnative-{rev}.tar.gz"))?;
    let out = ctx.payload.join("nxnative");
    say(format!("nxnative {} at {}", repo, &rev[..12]));
    untar(&tarball, &out, 1, &|_| true)?;
    for dir in ["common", ctx.platform] {
        let d = ctx.root.join("patches/nxnative").join(dir);
        let mut patches: Vec<PathBuf> = fs::read_dir(&d).map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "patch")).collect()).unwrap_or_default();
        patches.sort();
        for p in patches {
            say(format!("applying {}", p.strip_prefix(&ctx.root).unwrap_or(&p).display()));
            nxcore::patch::apply(&out, &fs::read_to_string(&p)?).with_context(|| format!("{} does not apply", p.display()))?;
        }
    }
    fs::write(out.join(".nxnativev2-revision"), format!("{repo}\t{rev}\n"))?;
    Ok(())
}
fn swift_object(ctx: &Ctx, sdk_dir: &Path) -> Result<()> {
    let sources = ctx.payload.join("nxnative/dm/ios/Sources");
    let mut files: Vec<PathBuf> = fs::read_dir(&sources)?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "swift")).collect();
    if files.is_empty() {
        return Ok(());
    }
    files.sort();
    let sdk = fs::read_dir(sdk_dir)
        .with_context(|| format!("no SDK folder {} (extract Xcode.xip with the app or `nxnative sdk` first, or pass --sdk-dir)", sdk_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.file_name().unwrap_or_default().to_string_lossy().starts_with("iPhoneOS") && p.join("SDKSettings.json").is_file())
        .max()
        .context("no iPhoneOS SDK in the SDK folder")?;
    let res = sdk_dir.join("swift-resources");
    ensure!(res.join("iphoneos").is_dir(), "no Swift resources in {}", res.display());
    say(format!("Swift sources → arm64 iOS object ({})", files.iter().map(|f| f.file_name().unwrap().to_string_lossy()).collect::<Vec<_>>().join(", ")));
    let out = ctx.payload.join("prebuilt/swift_DmIOS.o");
    fs::create_dir_all(out.parent().unwrap())?;
    // the same command as dm/ios/tools/build_app_linux.py compile_swift
    run(Command::new("swiftc")
        .args(["-target", &format!("arm64-apple-ios{MIN_IOS}"), "-sdk"])
        .arg(&sdk)
        .arg("-resource-dir")
        .arg(&res)
        .args(["-Xfrontend", "-enable-cross-import-overlays", "-parse-as-library", "-module-name", "DmIOS", "-O", "-wmo", "-c"])
        .args(&files)
        .arg("-o")
        .arg(&out))?;
    Ok(())
}
fn shrink(ctx: &Ctx) -> Result<()> {
    let py = ctx.payload.join("python");
    let lib = if ctx.windows { "Lib" } else { "lib/python3.13" };
    for rel in ["idlelib", "tkinter", "turtledemo", "ensurepip", "site-packages/pip"] {
        let _ = fs::remove_dir_all(py.join(lib).join(rel));
    }
    for rel in ["share", "include", "tcl"] {
        let _ = fs::remove_dir_all(py.join(rel));
    }
    for e in fs::read_dir(py.join(if ctx.windows { "DLLs" } else { "lib" }))?.filter_map(|e| e.ok()) {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with("tcl") || n.starts_with("tk") || n.starts_with("itcl") || n.starts_with("thread") || n.starts_with("_tkinter") {
            let _ = if e.path().is_dir() { fs::remove_dir_all(e.path()) } else { fs::remove_file(e.path()) };
        }
    }
    // bytecode caches (rebuilt on demand); binaries are left as they are: Python comes stripped, LLVM's release nearly so, and binutils'
    // strip broke python-build-standalone's interpreter
    let mut stack = vec![ctx.payload.clone()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d)?.filter_map(|e| e.ok()) {
            let p = e.path();
            if e.file_type()?.is_dir() {
                if p.file_name().is_some_and(|n| n == "__pycache__") {
                    fs::remove_dir_all(&p)?;
                } else {
                    stack.push(p);
                }
            }
        }
    }
    Ok(())
}
fn pack(ctx: &Ctx, exe: &Path, out: &Path) -> Result<()> {
    let archive = ctx.payload.with_extension("tar.zst");
    if !archive.is_file() {
        say("packing the payload (tar + zstd)");
        let mut enc = zstd::stream::write::Encoder::new(File::create(&archive)?, 19)?;
        enc.multithread(std::thread::available_parallelism().map_or(4, |n| n.get() as u32))?;
        let mut tar = tar::Builder::new(enc);
        tar.follow_symlinks(false);
        tar.mode(tar::HeaderMode::Deterministic);
        tar.append_dir_all(".", &ctx.payload)?;
        tar.into_inner()?.finish()?.sync_all()?;
    }
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    let mut f = File::open(&archive)?;
    let mut buf = vec![0u8; 8 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sha2::Digest::update(&mut hasher, &buf[..n]);
    }
    let hash: String = sha2::Digest::finalize(hasher).iter().take(8).map(|b| format!("{b:02x}")).collect();
    let len = fs::metadata(&archive)?.len();

    // written beside and renamed over the old one: a running copy keeps its file (copying into it fails with "Text file busy")
    fs::create_dir_all(out.parent().unwrap())?;
    let part = out.with_extension("partial");
    fs::copy(exe, &part)?;
    let mut o = fs::OpenOptions::new().append(true).open(&part)?;
    std::io::copy(&mut File::open(&archive)?, &mut o)?;
    o.write_all(&len.to_le_bytes())?;
    o.write_all(hash.as_bytes())?;
    o.write_all(nxcore::bundle::MAGIC)?;
    o.sync_all()?;
    drop(o);
    fs::rename(&part, out)?;
    say(format!("{} ({:.0} MB, payload {:.0} MB, bundle {hash})", out.display(), fs::metadata(out)?.len() as f64 / 1e6, len as f64 / 1e6));
    Ok(())
}
