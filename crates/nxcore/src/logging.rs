//! Process setup shared by the app and the CLI: the TLS crypto provider and the log file.

use std::fs::OpenOptions;
use std::sync::Once;

use crate::store::Store;

static TLS: Once = Once::new();

/// reqwest is built (through isideload) without a TLS crypto provider: the process installs aws-lc-rs once, as iloader does. Without it
/// every connection to Apple and to the anisette server fails.
pub fn tls() {
    TLS.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// `<data>/logs/nxnativev2.log`: isideload's sign-in and install steps (debug) and everything else at info. NXNATIVE_LOG overrides the
/// filter (tracing-subscriber's EnvFilter syntax). Returns the log's path.
pub fn init(store: &Store) -> Option<std::path::PathBuf> {
    tls();
    let dir = store.root().join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("nxnativev2.log");
    let file = OpenOptions::new().create(true).append(true).open(&path).ok()?;
    let filter = tracing_subscriber::EnvFilter::try_from_env("NXNATIVE_LOG").unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,isideload=debug,idevice=info,nxcore=debug,nxapp=debug"));
    tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(std::sync::Mutex::new(file)).try_init().ok()?;
    tracing::info!("nxnativev2 {} started", env!("CARGO_PKG_VERSION"));
    Some(path)
}
