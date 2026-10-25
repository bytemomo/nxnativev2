//! Apple ID sign-in, devices and installing, with isideload (the library of iloader) and idevice (usbmuxd).

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use futures::FutureExt;
use futures::future::BoxFuture;
use idevice::{IdeviceService, lockdown::LockdownClient, provider::UsbmuxdProvider, usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection}};
use isideload::{
    anisette::remote_v3::RemoteV3AnisetteProvider,
    auth::apple_account::{AppleAccount, TwoFactorCallbackParams, TwoFactorCallbackResponse},
    dev::{app_ids::AppIdsApi, certificates::CertificatesApi, developer_session::DeveloperSession},
    sideload::{
        SideloaderBuilder,
        builder::{ExtensionsBehavior, ExtensionsBehaviorSelection, MaxCertsBehavior},
        cert_identity::CertificateIdentity,
        sideloader::Sideloader,
    },
    util::{
        callbacks::{ExtensionsBehaviorCallbackBox, MaxCertsCallbackBox},
        fs_storage::FsStorage,
        keyring_storage::KeyringStorage,
        storage::SideloadingStorage,
    },
};

use crate::settings::Settings;
use crate::store::Store;

const SERVICE: &str = "nxnativev2";

/// What Apple asks for during sign-in
#[derive(Debug, Clone)]
pub struct TwoFactorPrompt {
    /// The last attempt's error (a wrong code, too many codes sent, ...)
    pub last_error: Option<String>,
    /// The code was sent by SMS (else to the trusted devices)
    pub sms: bool,
    /// The account's trusted phone numbers: (ID, "+39 ••• ••• ••50"); empty when Apple does not list them
    pub numbers: Vec<(u32, String)>,
}

#[derive(Debug, Clone)]
pub enum TwoFactorAnswer {
    Code(String),
    /// Send the code by SMS to the trusted number with this ID (also when Apple did not list the numbers: IDs start at 1)
    SmsTo(u32),
    ToDevices,
    Resend,
    Abort,
}

pub type TwoFactorAsk = Arc<dyn Fn(TwoFactorPrompt) -> BoxFuture<'static, TwoFactorAnswer> + Send + Sync>;

/// The account already has the most development certificates allowed: the names and serials it has; answer which serials to revoke (none = stop)
pub type CertsAsk = Arc<dyn Fn(Vec<(String, String)>) -> BoxFuture<'static, Vec<String>> + Send + Sync>;

pub struct Session {
    sideloader: Sideloader<MaxCertsCallbackBox, ExtensionsBehaviorCallbackBox>,
}

fn storage(store: &Store) -> Box<dyn SideloadingStorage> {
    if credentials::available() { Box::new(KeyringStorage::new(SERVICE.into())) } else { Box::new(FsStorage::new(store.session_dir())) }
}

pub(crate) fn report(r: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("{r}")
}

pub async fn login(store: &Store, settings: &Settings, email: &str, password: &str, ask: TwoFactorAsk, certs: CertsAsk) -> Result<Session> {
    crate::logging::tls();
    let email = email.trim().to_lowercase();
    let tfa = move |p: TwoFactorCallbackParams| {
        let ask = ask.clone();
        async move {
            let prompt = TwoFactorPrompt {
                last_error: p.last_error,
                sms: p.sms,
                numbers: p.numbers.iter().map(|n| (n.id, n.number_with_dial_code.clone())).collect(),
            };
            Ok(match ask(prompt).await {
                TwoFactorAnswer::Code(c) => TwoFactorCallbackResponse::SubmitCode(c.trim().replace(' ', "")),
                TwoFactorAnswer::SmsTo(id) => TwoFactorCallbackResponse::SendSms(id),
                TwoFactorAnswer::ToDevices => TwoFactorCallbackResponse::SendToDevices,
                TwoFactorAnswer::Resend => TwoFactorCallbackResponse::ResendCode,
                TwoFactorAnswer::Abort => TwoFactorCallbackResponse::Abort,
            })
        }
        .boxed()
    };
    let anisette = RemoteV3AnisetteProvider::default()
        .map_err(report)?
        .set_serial_number("0".into())
        .set_storage(storage(store))
        .set_url(&settings.anisette_url);
    tracing::info!(account = %email, anisette = %settings.anisette_url, "sign-in");
    let mut account: AppleAccount = AppleAccount::builder(&email)
        .anisette_provider(anisette)
        .login(password, Box::new(tfa))
        .await
        .map_err(report)?;
    let dev_session = DeveloperSession::from_account(&mut account).await.map_err(report)?;
    tracing::info!(account = %email, "sign-in: done");

    let max_certs: MaxCertsCallbackBox = Box::new(move |list| {
        let certs = certs.clone();
        Box::pin(async move {
            let named = list
                .iter()
                .map(|c| (c.machine_name.clone().or(c.name.clone()).unwrap_or_default(), c.serial_number.clone().unwrap_or_default()))
                .collect();
            let chosen = certs(named).await;
            Ok((!chosen.is_empty()).then_some(chosen))
        })
    });
    let sideloader = SideloaderBuilder::new(dev_session, email)
        .machine_name(SERVICE.into())
        .storage(storage(store))
        .extensions_behavior(ExtensionsBehaviorSelection::Fixed(ExtensionsBehavior::ReuseMain))
        .max_certs_behavior(MaxCertsBehavior::Prompt(max_certs))
        .build();
    Ok(Session { sideloader })
}

impl Session {
    pub fn email(&self) -> &str {
        self.sideloader.get_email()
    }

    /// The account's development certificate (the one this app signs with) as a PKCS#12 archive protected by `password`
    pub async fn certificate_p12(&mut self, store: &Store, password: &str) -> Result<Vec<u8>> {
        let team = self.sideloader.get_team().await.map_err(report)?;
        let email = self.email().to_string();
        let storage = storage(store);
        let id = CertificateIdentity::retrieve(
            SERVICE,
            &email,
            self.sideloader.get_dev_session(),
            &team,
            storage.as_ref(),
            &MaxCertsBehavior::<MaxCertsCallbackBox>::Error,
        )
        .await
        .map_err(report)?;
        id.as_p12(password).await.map_err(report)
    }

    /// The development certificate that belongs to a stored signing key, as a PKCS#12 archive (empty `password`: LiveContainer and SideStore
    /// accept it). Read-only, as p12_export: the key of this app, else iloader's (`keyring_service`), matched against Apple's list of the team's
    /// certificates; no certificate is made or revoked, so whatever was signed with it stays valid. Returns the archive and whose key it was.
    pub async fn existing_certificate_p12(&mut self, store: &Store, password: &str) -> Result<(Vec<u8>, &'static str)> {
        let (id, who) = self.signing_identity(store).await?;
        Ok((id.as_p12(password).await.map_err(report)?, who))
    }

    /// The account's details for the app: team and when the signing certificate (the one of this app's or iloader's key) expires
    pub async fn account(&mut self, store: &Store) -> Result<AccountInfo> {
        let team = self.sideloader.get_team().await.map_err(report)?;
        let expires = match self.signing_identity(store).await {
            Ok((id, _)) => Some(std::time::UNIX_EPOCH + id.certificate.tbs_certificate.validity.not_after.to_unix_duration()),
            Err(_) => None,
        };
        Ok(AccountInfo { email: self.email().to_string(), team_id: team.team_id.clone(), certificate_expires: expires })
    }

    /// The certificate of the key this app (or iloader) keeps, matched against Apple's list of the team's certificates (read-only)
    async fn signing_identity(&mut self, store: &Store) -> Result<(CertificateIdentity, &'static str)> {
        use rsa::{RsaPrivateKey, pkcs1::EncodeRsaPublicKey, pkcs8::DecodePrivateKey};
        use sha2::Digest;
        use x509_cert::der::Decode;
        let email = self.email().to_string();
        let key_name = format!("{}/key", sha2::Sha256::digest(email.as_bytes()).iter().map(|b| format!("{b:02x}")).collect::<String>());
        let sources: [(&'static str, Box<dyn SideloadingStorage>); 2] = [("nxnativev2", storage(store)), ("iloader", Box::new(KeyringStorage::new("iloader".into())))];
        let team = self.sideloader.get_team().await.map_err(report)?;
        let certs = self.sideloader.get_dev_session().list_ios_certs(&team).await.map_err(report)?;
        for (who, st) in sources {
            let Ok(Some(der)) = st.retrieve_data(&key_name) else { continue };
            let key = RsaPrivateKey::from_pkcs8_der(&der).map_err(|e| anyhow!("{who}'s key: {e}"))?;
            let public = key.to_public_key().to_pkcs1_der().map_err(|e| anyhow!("{e}"))?.as_bytes().to_vec();
            for c in &certs {
                let Some(content) = c.cert_content.as_ref() else { continue };
                let Ok(x509) = x509_cert::Certificate::from_der(content.as_ref()) else { continue };
                if x509.tbs_certificate.subject_public_key_info.subject_public_key.as_bytes() == Some(public.as_slice()) {
                    let id = CertificateIdentity {
                        machine_id: c.machine_id.clone().unwrap_or_default(),
                        machine_name: c.machine_name.clone().unwrap_or_default(),
                        private_key: key,
                        certificate: x509,
                    };
                    tracing::info!(key = who, serial = %id.get_serial_number(), "found the certificate of the stored key");
                    return Ok((id, who));
                }
            }
        }
        Err(anyhow!(
            "no signing certificate of team {} belongs to a key of this app or of iloader: install something once with this account (it makes the certificate), then export",
            team.team_id
        ))
    }

    /// The account's App IDs (one per app installed with it). `memory_limit`: whether Apple lists the Increased Memory Limit for it, when its
    /// features say so (None: not told)
    pub async fn app_ids(&mut self) -> Result<Vec<AppIdInfo>> {
        let team = self.sideloader.get_team().await.map_err(report)?;
        let list = self.sideloader.get_dev_session().list_app_ids(&team, None).await.map_err(report)?;
        let mut out: Vec<AppIdInfo> = list
            .app_ids
            .into_iter()
            .map(|a| {
                let memory_limit = a.features.iter().find(|(k, _)| k.to_lowercase().contains("memory")).and_then(|(_, v)| v.as_boolean());
                AppIdInfo { id: a.app_id_id, identifier: a.identifier, name: a.name, memory_limit }
            })
            .collect();
        out.sort_by(|a, b| a.identifier.cmp(&b.identifier));
        tracing::info!(count = out.len(), "app IDs listed");
        Ok(out)
    }

    /// Adds the Increased Memory Limit capability to an App ID (what GetMoreRam does): the app gets it once it is signed again with a new
    /// provisioning profile (reinstalled, or refreshed in its sideloading tool). LiveContainer passes it on to the apps it runs.
    pub async fn add_memory_limit(&mut self, identifier: &str) -> Result<()> {
        let team = self.sideloader.get_team().await.map_err(report)?;
        let dev = self.sideloader.get_dev_session();
        let list = dev.list_app_ids(&team, None).await.map_err(report)?;
        let app = list.app_ids.into_iter().find(|a| a.identifier == identifier).ok_or_else(|| anyhow!("no App ID {identifier} in this account"))?;
        dev.add_increased_memory_limit(&team, &app).await.map_err(report)?;
        tracing::info!(app_id = identifier, "Increased Memory Limit added");
        Ok(())
    }

    /// The team ID (the suffix sideloaded bundle IDs get)
    pub async fn team_id(&mut self) -> Result<String> {
        Ok(self.sideloader.get_team().await.map_err(report)?.team_id)
    }
}

/// Apple ID passwords kept in the system keyring (Secret Service on Linux, Credential Manager on Windows)
pub mod credentials {
    use super::SERVICE;

    fn entry(email: &str) -> Option<keyring::Entry> {
        keyring::Entry::new(SERVICE, &format!("apple-id:{}", email.trim().to_lowercase())).ok()
    }

    /// Whether a system keyring answers. Checked once per process, by reading an entry that does not exist: no write, so a locked
    /// keyring is not asked to unlock just for the check (the Secret Service searches locked collections without a prompt)
    pub fn available() -> bool {
        static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *AVAILABLE.get_or_init(|| match keyring::Entry::new(SERVICE, "availability-probe").and_then(|e| e.get_password()) {
            Ok(_) | Err(keyring::Error::NoEntry) => true,
            Err(e) => {
                tracing::warn!("no system keyring ({e}): sign-in state is kept in the data folder");
                false
            }
        })
    }

    pub fn save(email: &str, password: &str) -> anyhow::Result<()> {
        entry(email).ok_or_else(|| anyhow::anyhow!("no system keyring"))?.set_password(password)?;
        Ok(())
    }

    pub fn load(email: &str) -> Option<String> {
        entry(email)?.get_password().ok()
    }

    pub fn forget(email: &str) {
        if let Some(e) = entry(email) {
            let _ = e.delete_credential();
        }
    }
}

#[derive(Debug, Clone)]
pub struct AccountInfo {
    pub email: String,
    pub team_id: String,
    pub certificate_expires: Option<std::time::SystemTime>,
}

#[derive(Debug, Clone)]
pub struct AppIdInfo {
    pub id: String,
    /// The bundle ID as registered (sideloaded apps carry the team ID: com.kdt.livecontainer.ABCDE12345)
    pub identifier: String,
    pub name: String,
    pub memory_limit: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub udid: String,
    pub name: String,
    pub version: String,
    pub usb: bool,
}

async fn usbmuxd() -> Result<UsbmuxdConnection> {
    UsbmuxdConnection::default().await.map_err(|e| {
        anyhow!(
            "cannot reach usbmuxd ({e}): {}",
            if cfg!(windows) { "install iTunes or Apple Devices from the Microsoft Store" } else { "plug the device in (udev starts usbmuxd), or run: sudo systemctl start usbmuxd" }
        )
    })
}

/// The connected iPhones / iPads (a device that has not trusted this computer yet is left out)
pub async fn devices() -> Result<Vec<Device>> {
    crate::logging::tls();
    let mut mux = usbmuxd().await?;
    let addr = UsbmuxdAddr::from_env_var().map_err(report)?;
    let mut out = vec![];
    for d in mux.get_devices().await.map_err(report)? {
        let provider = d.to_provider(addr.clone(), SERVICE);
        let Ok(mut lockdown) = LockdownClient::connect(&provider).await else { continue };
        let mut get = async |k: &str| lockdown.get_value(Some(k), None).await.ok().and_then(|v| v.as_string().map(str::to_string));
        let name = get("DeviceName").await.unwrap_or_else(|| d.udid.clone());
        let version = get("ProductVersion").await.unwrap_or_default();
        out.push(Device { udid: d.udid.clone(), name, version, usb: matches!(d.connection_type, Connection::Usb) });
    }
    Ok(out)
}

pub(crate) async fn provider(udid: &str) -> Result<UsbmuxdProvider> {
    let mut mux = usbmuxd().await?;
    let d = mux.get_device(udid).await.map_err(report).context("the device is no longer connected")?;
    Ok(d.to_provider(UsbmuxdAddr::from_env_var().map_err(report)?, SERVICE))
}

/// Signs `ipa` with the account and installs it on the device. `progress` gets 0..1.
/// With `increased_memory_limit` the app ID asks for the Increased Memory Limit capability; if Apple refuses it, the app is installed without.
pub async fn install(session: &mut Session, udid: &str, ipa: &Path, increased_memory_limit: bool, progress: Arc<dyn Fn(f32) + Send + Sync>) -> Result<()> {
    tracing::info!(device = udid, ipa = %ipa.display(), increased_memory_limit, account = session.email(), "install: signing and installing");
    let p = provider(udid).await?;
    let cb = |f: f32| {
        let progress = progress.clone();
        async move { progress(f) }
    };
    let first = session.sideloader.install_app(&p, ipa.to_path_buf(), increased_memory_limit, Some(cb), None).await;
    match first {
        Ok(_) => {
            tracing::info!(device = udid, "install: done");
            Ok(())
        }
        Err(e) if increased_memory_limit && e.to_string().to_lowercase().contains("increased memory") => {
            tracing::warn!("install: Apple refused the Increased Memory Limit ({e}); installing without it");
            let r = session.sideloader.install_app(&p, ipa.to_path_buf(), false, Some(cb), None).await.map_err(report);
            match &r {
                Ok(_) => tracing::info!(device = udid, "install: done (without the Increased Memory Limit)"),
                Err(e) => tracing::error!("install failed: {e:#}"),
            }
            r.map(|_| ())
        }
        Err(e) => {
            tracing::error!("install failed: {e:#}");
            Err(report(e))
        }
    }
}
