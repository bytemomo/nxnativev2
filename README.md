# nxnativev2

A desktop app that turns your own Nintendo Switch game files into an iPhone / iPad app and installs it: drop the game files and keys,
build the IPA with the [nxnative](https://github.com/bytemomo/nxnative) pipeline, then sign it with an Apple ID and install it, or export
it for [LiveContainer](https://github.com/LiveContainer/LiveContainer). One executable per platform carries everything the build needs.
Linux first; built to run on Windows too.

## What you provide

- **Game files**: the base game (`.nsp` / `.xci`), optionally updates and DLC, and your console's **`prod.keys`** (+ `title.keys`).
- **Apple's tools** (free with an Apple ID at developer.apple.com/download/all; Apple's licence does not allow bundling them):
  - **Xcode** as a `.xip` (any Xcode 26, e.g. `Xcode 26.6 Apple silicon.xip`): only the iPhoneOS SDK (~200 MB) is extracted from it.
  - **Metal Developer Tools for Windows** (6.x installer): the shader compiler. On Linux it runs under wine, in a prefix of the app's own.
- An **Apple ID** to sign the apps (a free one works: apps then need signing again every 7 days, and at most 3 at a time; LiveContainer
  lifts the 3-app limit).

Everything else is inside the executable (unpacked once into the data folder on the first build): the pinned nxnative revision with this
repository's patches applied, a portable Python with the pipeline's modules, the .NET runtime with nx_extract, the app's Swift launcher
compiled ahead of time, spirv-cross, and clang + lld. On Linux the system provides wine (Apple's Metal compiler runs under it), make and bash.

## Using it

1. **Apple tools** page: choose the Xcode `.xip` and the Metal installer once.
2. Drop the game files and `prod.keys` on the window (or **Add files…**). Each game appears in the sidebar.
3. **Build IPA**: the first time the app unpacks its build tools, then extraction, code rewrite, shaders and app (5–10 minutes the
   first time; much faster again).
4. **iPhone / iPad** page: sign in with your Apple ID (the password can be kept in the system keyring; the app then signs in by itself
   when it starts) and pick the connected device.
5. Then, from the game's page:
   - **Sign with your Apple ID and install** (the normal way). On the device: Settings → Privacy & Security → Developer Mode on,
     then Settings → General → VPN & Device Management → trust your Apple ID.
   - **Export for LiveContainer**: the IPA and the account's signing certificate (`<Apple ID>.p12`, no password) are copied into your
     Downloads folder. Move both to the device, import the certificate in LiveContainer (Settings → Import Certificate) and install the IPA
     there; LiveContainer signs it. The certificate is exported read-only (the one belonging to this app's signing key, else iloader's:
     nothing is created or revoked), and LiveContainer must itself be signed with the same Apple ID (**Install an IPA…** on the
     iPhone / iPad page, or iloader).

Sign-in: when no code pop-up appears, the code dialog can send an SMS to a specific trusted number ID (Apple does not always list the
numbers; IDs start at 1, a changed number may be 2), or use a code from Settings → your name → Sign-In & Security → Get Verification Code.

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
                  export       IPA and certificate into the Downloads folder
crates/nxcli    the same steps on the command line: nxnative check | setup | sdk | metal | import | games | build | devices |
                install | install-ipa | export | forget
crates/nxapp    the Slint desktop app
crates/xtask    cargo xtask bundle: the packaged executable (dist/)
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
cargo xtask bundle               # the packaged app: dist/nxnativev2-linux-x64
```

`cargo xtask bundle` needs, on the build machine: the .NET SDK 10 (to publish nx_extract), Swift (swiftc) with the iOS SDK and Swift
resources extracted by the app or `nxnative sdk` (from Xcode.xip; `--sdk-dir` to point elsewhere), git, cmake and ninja (spirv-cross).
Downloads (nxnative, Python, the .NET runtime, LLVM) are cached in `target/bundle-cache`.

On Wayland, the app opens through XWayland when it is available, because winit receives dropped files only on X11 / Windows;
`NXNATIVE_WAYLAND=1` keeps it on Wayland (sharper, but use **Add files…** instead of dropping).
