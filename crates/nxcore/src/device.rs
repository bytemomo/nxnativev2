//! What the app shows about a connected iPhone / iPad: its model, iOS, link, free space, Developer Mode and the user apps installed on it
//! (installation_proxy, AFC, lockdown over usbmuxd), and removing one of them.

use anyhow::{Context, Result};
use idevice::{IdeviceService, afc::AfcClient, installation_proxy::InstallationProxyClient, lockdown::LockdownClient};

use crate::apple::{provider, report};

#[derive(Debug, Clone, Default)]
pub struct InstalledApp {
    pub bundle_id: String,
    pub name: String,
    pub version: String,
    pub executable: String,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub udid: String,
    pub name: String,
    /// "iPad17,1"
    pub product_type: String,
    /// "iPad Pro 11-inch (M5)" (the product type when the table does not know it)
    pub model: String,
    pub ios: String,
    /// The USB link the device negotiated, in Mbit/s (Linux; None elsewhere or over the network)
    pub link_mbps: Option<u32>,
    pub free: Option<u64>,
    pub total: Option<u64>,
    pub developer_mode: Option<bool>,
    /// The user apps (not the system ones)
    pub apps: Vec<InstalledApp>,
}

/// The connected device's details (a second or so: it lists the installed apps)
pub async fn info(udid: &str) -> Result<DeviceInfo> {
    crate::logging::tls();
    let p = provider(udid).await?;
    let mut d = DeviceInfo { udid: udid.to_string(), ..Default::default() };
    if let Ok(mut l) = LockdownClient::connect(&p).await {
        let mut get = async |k: &str, domain: Option<&str>| l.get_value(Some(k), domain).await.ok();
        d.name = get("DeviceName", None).await.and_then(|v| v.as_string().map(str::to_string)).unwrap_or_default();
        d.product_type = get("ProductType", None).await.and_then(|v| v.as_string().map(str::to_string)).unwrap_or_default();
        d.ios = get("ProductVersion", None).await.and_then(|v| v.as_string().map(str::to_string)).unwrap_or_default();
        d.developer_mode = get("DeveloperModeStatus", Some("com.apple.security.mac.amfi")).await.and_then(|v| v.as_boolean());
    }
    d.model = model_name(&d.product_type);
    d.link_mbps = usb_link_mbps(udid);
    if let Ok(mut afc) = AfcClient::connect(&p).await {
        if let Ok(i) = afc.get_device_info().await {
            d.free = Some(i.free_bytes as u64);
            d.total = Some(i.total_bytes as u64);
        }
    }
    let mut ip = InstallationProxyClient::connect(&p).await.map_err(report).context("list the device's apps")?;
    for (bundle_id, info) in ip.get_apps(Some("User"), None).await.map_err(report)? {
        let get = |k: &str| info.as_dictionary().and_then(|d| d.get(k)).and_then(|v| v.as_string()).unwrap_or_default().to_string();
        let name = Some(get("CFBundleDisplayName")).filter(|s| !s.is_empty()).unwrap_or_else(|| get("CFBundleName"));
        d.apps.push(InstalledApp { name, version: get("CFBundleShortVersionString"), executable: get("CFBundleExecutable"), bundle_id });
    }
    d.apps.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(d)
}

/// Removes an app from the device (with its data)
pub async fn uninstall(udid: &str, bundle_id: &str) -> Result<()> {
    crate::logging::tls();
    let p = provider(udid).await?;
    let mut ip = InstallationProxyClient::connect(&p).await.map_err(report)?;
    tracing::info!(device = udid, bundle_id, "uninstall");
    ip.uninstall(bundle_id.to_string(), None).await.map_err(report).with_context(|| format!("remove {bundle_id}"))
}

/// The USB link a device negotiated (Linux: the sysfs entry whose serial is the UDID without its dash)
pub fn usb_link_mbps(udid: &str) -> Option<u32> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let want = udid.replace('-', "").to_lowercase();
    std::fs::read_dir("/sys/bus/usb/devices").ok()?.filter_map(|e| e.ok()).find_map(|e| {
        let serial = std::fs::read_to_string(e.path().join("serial")).ok()?;
        (serial.trim().replace('-', "").to_lowercase() == want).then(|| std::fs::read_to_string(e.path().join("speed")).ok()?.trim().parse::<f32>().ok().map(|s| s as u32))?
    })
}

/// The marketing name of a product type (the devices that run the games: recent iPads and iPhones)
pub fn model_name(product_type: &str) -> String {
    let name = match product_type {
        "iPad17,1" | "iPad17,2" => "iPad Pro 11-inch (M5)",
        "iPad17,3" | "iPad17,4" => "iPad Pro 13-inch (M5)",
        "iPad16,3" | "iPad16,4" => "iPad Pro 11-inch (M4)",
        "iPad16,5" | "iPad16,6" => "iPad Pro 13-inch (M4)",
        "iPad15,3" | "iPad15,4" => "iPad Air 11-inch (M3)",
        "iPad15,5" | "iPad15,6" => "iPad Air 13-inch (M3)",
        "iPad14,8" | "iPad14,9" => "iPad Air 11-inch (M2)",
        "iPad14,10" | "iPad14,11" => "iPad Air 13-inch (M2)",
        "iPad16,1" | "iPad16,2" => "iPad mini (A17 Pro)",
        "iPad15,7" | "iPad15,8" => "iPad (A16)",
        "iPad14,3" | "iPad14,4" => "iPad Pro 11-inch (4th generation)",
        "iPad14,5" | "iPad14,6" => "iPad Pro 12.9-inch (6th generation)",
        "iPad13,4" | "iPad13,5" | "iPad13,6" | "iPad13,7" => "iPad Pro 11-inch (3rd generation)",
        "iPad13,8" | "iPad13,9" | "iPad13,10" | "iPad13,11" => "iPad Pro 12.9-inch (5th generation)",
        "iPad13,16" | "iPad13,17" => "iPad Air (5th generation)",
        "iPad14,1" | "iPad14,2" => "iPad mini (6th generation)",
        "iPhone18,1" => "iPhone 17 Pro",
        "iPhone18,2" => "iPhone 17 Pro Max",
        "iPhone18,3" => "iPhone 17",
        "iPhone18,4" => "iPhone Air",
        "iPhone17,1" => "iPhone 16 Pro",
        "iPhone17,2" => "iPhone 16 Pro Max",
        "iPhone17,3" => "iPhone 16",
        "iPhone17,4" => "iPhone 16 Plus",
        "iPhone17,5" => "iPhone 16e",
        "iPhone16,1" => "iPhone 15 Pro",
        "iPhone16,2" => "iPhone 15 Pro Max",
        _ => "",
    };
    if name.is_empty() { product_type.to_string() } else { name.to_string() }
}

/// The short name for the sidebar ("iPad Pro", "iPhone 16 Pro")
pub fn short_name(model: &str) -> String {
    model.split(" (").next().unwrap_or(model).replace(" 11-inch", "").replace(" 13-inch", "").replace(" 12.9-inch", "")
}
