//! The data folder:
//!
//! ```text
//! <data>/games/<title id>/keys/          prod.keys, title.keys
//! <data>/games/<title id>/game_files/    base game, updates, DLC (NSP / XCI)
//! <data>/games/<title id>/build/         the pipeline's work folder (extraction, rewrite, shaders, app)
//! <data>/games/<title id>/<title id>.ipa the built app
//! <data>/apple/native_tools/sdk/         iPhoneOS SDK, from the user's Xcode.xip
//! <data>/apple/native_tools/metal/       Metal Developer Tools for Windows (on Linux inside a wine prefix)
//! <data>/apple/session/                  Apple sign-in state when no system keyring is available
//! <data>/tmp/                            temporary files (the IPA is unpacked here to be signed; /tmp may be a small RAM disk)
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn games_dir(&self) -> PathBuf {
        self.root.join("games")
    }

    pub fn game_dir(&self, id: u64) -> PathBuf {
        self.games_dir().join(format!("{id:016X}"))
    }

    pub fn keys_dir(&self, id: u64) -> PathBuf {
        self.game_dir(id).join("keys")
    }

    pub fn prod_keys(&self, id: u64) -> PathBuf {
        self.keys_dir(id).join("prod.keys")
    }

    pub fn files_dir(&self, id: u64) -> PathBuf {
        self.game_dir(id).join("game_files")
    }

    pub fn build_dir(&self, id: u64) -> PathBuf {
        self.game_dir(id).join("build")
    }

    pub fn ipa(&self, id: u64) -> PathBuf {
        self.game_dir(id).join(format!("{id:016X}.ipa"))
    }

    pub fn native_tools(&self) -> PathBuf {
        self.root.join("apple").join("native_tools")
    }

    pub fn sdk_dir(&self) -> PathBuf {
        self.native_tools().join("sdk")
    }

    /// The installed iPhoneOS SDK (the versioned name, e.g. `iPhoneOS26.5.sdk`), if any
    pub fn sdk(&self) -> Option<PathBuf> {
        let mut sdks: Vec<_> = fs::read_dir(self.sdk_dir())
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                let n = p.file_name().unwrap_or_default().to_string_lossy();
                n.starts_with("iPhoneOS") && n.ends_with(".sdk") && n != "iPhoneOS.sdk" && p.join("SDKSettings.json").is_file()
            })
            .collect();
        sdks.sort();
        sdks.pop()
    }

    /// Xcode's Swift resource folder (extracted beside the SDK), for the app's Swift sources
    pub fn swift_resources(&self) -> Option<PathBuf> {
        let d = self.sdk_dir().join(crate::xip::SWIFT_RESOURCES);
        d.join("iphoneos").is_dir().then_some(d)
    }

    pub fn metal_dir(&self) -> PathBuf {
        self.native_tools().join("metal")
    }

    pub fn session_dir(&self) -> PathBuf {
        self.root.join("apple").join("session")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }
}
