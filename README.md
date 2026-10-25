# nxnativev2

A desktop app that turns your own Nintendo Switch game files into an iPhone / iPad app and installs it: drop the game files and keys,
build the IPA with the [nxnative](https://github.com/bytemomo/nxnative) pipeline, then sign it with an Apple ID and install it, or export
it for [LiveContainer](https://github.com/LiveContainer/LiveContainer). One executable per platform carries everything the build needs:
Linux (`nxnativev2-linux-x64`) and Windows (`nxnativev2-windows-x64.exe`).

## What you provide

- **Game files**: the base game (`.nsp` / `.xci`), optionally updates and DLC, and your console's **`prod.keys`** (+ `title.keys`).
- **Apple's tools** (free with an Apple ID at developer.apple.com/download/all; Apple's licence does not allow bundling them):
  - **Xcode** as a `.xip` (any Xcode 26, e.g. `Xcode 26.6 Apple silicon.xip`): only the iPhoneOS SDK (~200 MB) is extracted from it.
  - **Metal Developer Tools for Windows** (6.x installer): the shader compiler. On Linux it runs under wine, in a prefix of the app's own.
- An **Apple ID** to sign the apps (a free one works: apps then need signing again every 7 days, and at most 3 at a time; LiveContainer
  lifts the 3-app limit).

Everything else is inside the executable (unpacked once into the data folder on the first build): the pinned nxnative revision with this
repository's patches applied, a portable Python with the pipeline's modules, the .NET runtime with nx_extract, the app's Swift launcher
compiled ahead of time, spirv-cross, clang + lld, and DejaVu Sans (the stand-in for the Switch's shared fonts, so no system font is
needed; a TrueType font of the computer that covers more languages, such as Japanese, Chinese and Korean, is taken instead). On Linux the system provides wine (Apple's Metal compiler runs under it); Windows runs
Apple's Metal compiler natively and needs nothing else for the build. Installing on a device needs Apple's device service: on Windows the
Apple Devices app (Microsoft Store) or iTunes; on Linux usbmuxd.

## Using it

1. **Apple tools** page: choose the Xcode `.xip` and the Metal installer once.
2. Drop the game files and `prod.keys` on the window (or **Add files…**). Each game appears in the sidebar.
   Any other iOS app's `.ipa` can be dropped too: it gets an entry of its own (name, version and icon from the IPA), with nothing to
   build, and installs or copies into LiveContainer like a game. An IPA built here before goes back to its game.
3. **Build IPA**: the first time the app unpacks its build tools, then extraction, code rewrite, shaders and app (5–10 minutes the
   first time; much faster again).
4. **iPhone / iPad** page: sign in with your Apple ID (the password can be kept in the system keyring; the app then signs in by itself
   when it starts) and pick the connected device.
5. Then, from the game's page:
   - **Sign with your Apple ID and install** (the normal way). On the device: Settings → Privacy & Security → Developer Mode on,
     then Settings → General → VPN & Device Management → trust your Apple ID.
   - **Share with LiveContainer over Wi-Fi**: the app serves a page on this computer and shows its QR code (the address has a
     random token: only someone with the link gets the files). On the device (same Wi-Fi) scan it or open it in Safari: one button
     imports the account's signing certificate into LiveContainer (`livecontainer://certificate`, once per Apple ID), one makes
     LiveContainer download the game from this computer and install it (`livecontainer://install?url=`); LiveContainer signs it.
     The certificate is exported read-only (the one belonging to this app's signing key, else iloader's: nothing is created or
     revoked), and LiveContainer must itself be signed with the same Apple ID (**Install an IPA…** on the iPhone / iPad page, or
     iloader). **Save IPA + certificate to Downloads** copies both files instead. The firewall must let the device reach port 8642.
   - **Copy into LiveContainer over USB**: much faster than Wi-Fi, no sign-in. The device cannot reach this computer through the
     cable (usbmuxd only connects the other way), so the app writes the game straight into LiveContainer's
     `Documents/Applications/<bundle ID>.app` (house_arrest + AFC, as Finder's file sharing; four streams at once). Close and reopen
     LiveContainer: it lists the game, and patches and signs it at its first launch with the certificate imported in it (Share over
     Wi-Fi imports this account's once). A game already there is replaced and keeps its `LCAppInfo.plist`, so its data and settings.

**More RAM for LiveContainer** (iPhone / iPad page, **Give LiveContainer more RAM**; `nxnative lc-more-ram`): Apple's Increased
Memory Limit, for every game LiveContainer runs. The permission lives in the app's signature, so restarting is not enough: in one go the
app turns it on for LiveContainer's App ID and installs LiveContainer again over itself, the same version and kind (with SideStore or not)
downloaded from its GitHub release; its games and data stay. Sign in with the Apple ID LiveContainer was installed with.

The Apple ID password and the sign-in state are kept in the system keyring (Secret Service on Linux, Credential Manager on Windows).
On Linux the keyring is encrypted with its own password: it asks for it once per login unless the login unlocks it (GNOME Keyring:
give the "login" keyring your account password and make it the default; it then opens with your session).

Sign-in: when no code pop-up appears, the code dialog can send an SMS to a specific trusted number ID (Apple does not always list the
numbers; IDs start at 1, a changed number may be 2), or use a code from Settings → your name → Sign-In & Security → Get Verification Code.

**Updates** (Settings › Updates; `nxnative update`): the app and its bundle (the build tools: nxnative, Python, .NET, LLVM, ...) update
on their own from this repository's GitHub releases. The app looks for one when it starts (it installs only when asked; the switch is
in the same group). A release's `manifest.json` is signed with the release key (ed25519); the app trusts only that key, checks every
download's size and SHA-256 against the manifest, and never installs an older version. A new bundle is used from the next build, a new
app from the next start. Each bundle comes with its SBOM (`bundle-<platform>.cdx.json`, CycloneDX); releases made in CI are also attested
(`gh attestation verify bundle-linux-x64.tar.zst --repo bytemomo/nxnativev2`).

## Layout

```
crates/nxcore   everything the app does, no UI
                  xip          Xcode.xip → iPhoneOS SDK + Xcode's Swift resources (xar, pbzx, cpio; hard links resolved in two passes)
                  metal        the Metal compiler (wine on Linux)
                  bundle       the payload appended to the executable: found, unpacked once, its paths and environment
                  patch        applying the nxnative patches (git-format diffs) without git
                  nsp, games   game files, keys and the data folder
                  pipeline     runs the nxnative script (JSON progress)
                  apple        Apple ID sign-in (isideload), keyring credentials, devices, signing and installing, the read-only certificate export
                  serve        the Wi-Fi share for LiveContainer: a page with the certificate and install links, the IPA, a QR code
                  lc_usb       the USB copy into LiveContainer's Documents (house_arrest + AFC)
                  export       IPA and certificate into the Downloads folder
                  logging      the TLS crypto provider and the log file (<data>/logs/nxnativev2.log)
crates/nxcli    the same steps on the command line: nxnative check | setup | sdk | metal | import | games | build | devices |
                install | install-ipa | serve | export | diag | update | lc-list | lc-copy | lc-more-ram | app-ids | more-ram | forget
                (build --debug: an IPA with the diagnostic switches on, local play's DM_LAN_DEBUG among them; runtime/README.md)
crates/nxapp    the Slint desktop app
crates/xtask    cargo xtask bundle [--target linux|windows]: the packaged executable (dist/), both built on Linux; release, keygen,
                swift-object: the signed GitHub releases the app updates from
crates/launchers  xcrun.exe and dotnet-dll-launcher.exe: what the pipeline runs by name on Windows
patches/nxnative  UPSTREAM (repository and revision); common/ for every platform, linux/ and windows/ per platform
```

Data folder: Linux `~/.local/share/nxnativev2`, Windows `%LOCALAPPDATA%\nxnativev2\data`:

```
games/<title id>/keys/  game_files/  build/  <title id>.ipa
apple/native_tools/sdk/  apple/native_tools/metal/  apple/session/
bundle/<hash>/   tmp/
```

Settings: Linux `~/.config/nxnativev2/settings.toml`, Windows `%APPDATA%\nxnativev2\config\settings.toml` (`nxnative_repo` and `python`
point at your own checkout / Python instead of the downloaded ones, for nxnative development).

## Moving to a newer nxnative

Change `rev` in `patches/nxnative/UPSTREAM` (a revision whose game profiles hold the current shader chain version), make the patches apply
again (in a checkout of the new revision: apply them, fix, and `git diff --binary <rev>` into the patch files), then `cargo xtask bundle`.

## Building

```
cargo build --release            # development: target/release/nxnativev2 (app) and nxnative (CLI), no bundle: set nxnative_repo / python
cargo xtask bundle                    # the packaged app: dist/nxnativev2-linux-x64
cargo xtask bundle --target windows   # dist/nxnativev2-windows-x64.exe (cross-compiled: rustup target add x86_64-pc-windows-gnu, mingw-w64)
```

`cargo xtask bundle` needs, on the build machine: the .NET SDK 10 (to publish nx_extract), Swift (swiftc) with the iOS SDK and Swift
resources extracted by the app or `nxnative sdk` (from Xcode.xip; `--sdk-dir` to point elsewhere), git, cmake and ninja (spirv-cross); for
Windows also the Rust target x86_64-pc-windows-gnu, mingw-w64 (gcc, g++) and pip (Windows wheels of the Python modules).
Downloads (nxnative, Python, the .NET runtime, LLVM) are cached in `target/bundle-cache`.

### Releases

`just` lists the everyday commands (`justfile`): `build` / `build-release` (development builds), `dist` / `dist-all` (the packaged
app and CLI), `check`, and the releases below. Their secrets come from 1Password: the release key and the GitHub token by reference
(`op://nxnativev2/...`), read only for the command that uses them.

```
just keycheck      # the release key in 1Password is the one clients trust
just plan          # what the next release would build, and why (nothing is built)
just publish       # what changed since the latest release, built, signed and published: running apps update to it
just release       # the same into dist/release/ without publishing; `just upload` publishes it afterwards
just ci-secret     # the release key as the repository secret CI signs with
just ci-release    # a release in GitHub Actions now (a commit on main with [ci] in its message does the same)
```

Releases are incremental: `xtask release` hashes each component's inputs (each platform's bundle: nxnative's
revision and patches, `runtime/`, how xtask builds it, the bundle API; the app: `crates/nxcore`, `nxapp`, `nxcli`, `Cargo.toml`, `Cargo.lock`)
and builds only the components whose hash differs from the latest release's; the rest are carried over, and the manifest names the release
that holds each file. Nothing changed: no release. An app installs only bundles whose API it runs (`BUNDLE_API` in `crates/nxcore/src/update.rs`:
raise it when how the app runs a bundle changes).

Versions come from the release: the bundle's is the release number (its UTC time, YYYYMMDDHHMM), the app's `<major>.<release
number>` (xtask stamps the number into the build), so every release is newer than the last and clients update to it. Only the major
(`version` in `Cargo.toml`) is raised by hand, for a change clients should notice. Development builds (`cargo build`, `just build`)
report `<major>.0-dev` and never replace themselves. The GitHub token needs, on this repository: Contents read and write (releases),
Secrets read and write (`ci-secret`), Actions read and write (`ci-release`, `ci-status`).

Without just: `cargo xtask keygen` (once: the release key), `cargo xtask keycheck`, `cargo xtask release [--publish]`, with the key in
`NXNATIVEV2_SIGNING_KEY` (hex or an `op://` reference). CI (`.github/workflows/release.yml`) builds the Swift object on a macOS runner
with Xcode's SDK and the rest on Ubuntu, signs with the repository secret `NXNATIVEV2_SIGNING_KEY`, and attests every asset and SBOM.
`NXNATIVEV2_UPDATE_URL` points the app at another server with the same layout (testing).

On Wayland, the app opens through XWayland when it is available, because winit receives dropped files only on X11 / Windows;
`NXNATIVE_WAYLAND=1` keeps it on Wayland (sharper, but use **Add files…** instead of dropping).
