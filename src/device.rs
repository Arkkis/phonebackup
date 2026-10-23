// Device discovery, pairing, Wi-Fi sync, encryption, backup and restore on top of the `idevice` crate.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use idevice::heartbeat::HeartbeatClient;
use idevice::lockdown::LockdownClient;
use idevice::mobilebackup2::{
    BackupDelegate, BackupProgress, FsBackupDelegate, MobileBackup2Client, RestoreOptions,
};
use idevice::pairing_file::PairingFile;
use idevice::provider::{IdeviceProvider, TcpProvider};
use idevice::usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice};
use idevice::{IdeviceError, IdeviceService};

const LABEL: &str = "phonebackup";
const MOBDEV2: &str = "_apple-mobdev2._tcp.local.";

pub type Log<'a> = &'a (dyn Fn(String) + Send + Sync);
pub type Prog<'a> = &'a (dyn Fn(BackupProgress) + Send + Sync);

#[derive(Clone)]
pub enum Transport {
    Usb(UsbmuxdDevice),
    Wifi(IpAddr),
}

/// What the phone reports about itself. The `Option`s need a trusted (paired) session to read.
#[derive(Clone, Default)]
pub struct Info {
    pub name: String,
    pub ios: String,
    pub paired: bool,
    pub encrypted: Option<bool>,
    pub wifi_sync: Option<bool>,
    pub find_my: Option<bool>,
}

#[derive(Clone)]
pub struct Device {
    pub udid: String,
    pub transport: Transport,
    pub info: Info,
}

impl Device {
    pub fn is_usb(&self) -> bool {
        matches!(self.transport, Transport::Usb(_))
    }
}

/// Why no phone is visible on USB.
#[derive(Clone, Copy, PartialEq)]
pub enum Hint {
    NoUsbmuxd,
    MuxDown,
    Waiting,
    NoCable,
}

pub struct Scan {
    pub devices: Vec<Device>,
    /// Names of phones with Wi-Fi sync on that aren't advertising right now.
    pub offline: Vec<String>,
    pub hint: Option<Hint>,
}

pub struct BackupEntry {
    pub udid: String,
    pub name: String,
    pub date: String,
    pub ios: String,
    pub size: u64,
    pub encrypted: bool,
}

/// Pairing records copied out of usbmuxd so the phone can be reached over Wi-Fi without it.
fn pairs_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
    });
    base.join("phonebackup/pairs")
}

fn pair_path(udid: &str, ext: &str) -> PathBuf {
    pairs_dir().join(format!("{udid}.{ext}"))
}

fn known_name(udid: &str) -> Option<String> {
    std::fs::read_to_string(pair_path(udid, "name")).ok()
}

/// Turns library errors into something a person can act on.
pub fn friendly(e: IdeviceError) -> String {
    match e {
        IdeviceError::PasswordProtected | IdeviceError::DeviceLocked => {
            "The iPhone is locked. Unlock it and try again.".into()
        }
        IdeviceError::UserDeniedPairing => {
            "Don't Trust was tapped on the iPhone. Unplug it, plug it back in, and tap Trust.".into()
        }
        IdeviceError::PairingDialogResponsePending => "Waiting for Trust to be tapped on the iPhone.".into(),
        IdeviceError::InvalidHostID | IdeviceError::SessionInactive => {
            "The iPhone no longer trusts this computer. Plug it in over USB and click Trust this computer.".into()
        }
        IdeviceError::CanceledByUser => "Cancelled on the iPhone.".into(),
        IdeviceError::NotFound => "The backup folder for that phone wasn't found.".into(),
        IdeviceError::Usbmuxd(_) | IdeviceError::DeviceNotFound => {
            "Lost the USB connection. Check the cable and try again.".into()
        }
        IdeviceError::Timeout => lost(),
        IdeviceError::Socket(io) => match io.kind() {
            ErrorKind::ConnectionRefused | ErrorKind::TimedOut | ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable => {
                "Couldn't reach the iPhone. Over Wi-Fi it must be unlocked and on the same network as this PC.".into()
            }
            ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => lost(),
            _ => format!("Couldn't talk to the iPhone ({io})."),
        },
        e => format!("Something went wrong ({e})."),
    }
}

fn lost() -> String {
    "The connection to the iPhone was lost. Keep it unlocked and connected, then try again.".into()
}

fn io_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Watches `_apple-mobdev2._tcp` and keeps device lookups cheap between scans.
pub struct Scanner {
    wifi: Arc<Mutex<HashMap<String, IpAddr>>>,
    cache: Mutex<HashMap<String, Info>>,
}

impl Scanner {
    pub fn new() -> Arc<Self> {
        let s = Arc::new(Self { wifi: Default::default(), cache: Default::default() });
        watch_wifi(s.wifi.clone());
        s
    }

    /// Drops cached device info so the next scan re-reads it (after pairing, encryption changes, …).
    pub fn refresh(&self) {
        self.cache.lock().unwrap().clear();
    }

    pub async fn scan(&self) -> Scan {
        let mut found = Vec::new();
        let mux_ok = match UsbmuxdConnection::default().await {
            Ok(mut mux) => {
                for d in mux.get_devices().await.unwrap_or_default() {
                    if d.connection_type == Connection::Usb {
                        found.push((d.udid.clone(), Transport::Usb(d)));
                    }
                }
                true
            }
            Err(_) => false,
        };
        let usb_count = found.len();
        let wifi: Vec<(String, IpAddr)> = self.wifi.lock().unwrap().iter().map(|(u, ip)| (u.clone(), *ip)).collect();
        for (udid, ip) in wifi {
            if !found.iter().any(|(u, _)| *u == udid) {
                found.push((udid, Transport::Wifi(ip)));
            }
        }

        let mut devices = Vec::new();
        for (udid, transport) in found {
            let cached = self.cache.lock().unwrap().get(&udid).cloned();
            let info = match cached {
                Some(i) => i,
                None => {
                    let i = probe(&udid, &transport).await;
                    self.cache.lock().unwrap().insert(udid.clone(), i.clone());
                    i
                }
            };
            devices.push(Device { udid, transport, info });
        }

        let mut offline = Vec::new();
        for entry in std::fs::read_dir(pairs_dir()).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "wifi")
                && let Some(udid) = path.file_stem().and_then(|s| s.to_str())
                && !devices.iter().any(|d| d.udid == udid)
            {
                offline.push(known_name(udid).unwrap_or_else(|| "iPhone".into()));
            }
        }

        let hint = (usb_count == 0).then(|| {
            if !Path::new("/usr/bin/usbmuxd").exists() {
                Hint::NoUsbmuxd
            } else if apple_on_usb() {
                if mux_ok { Hint::Waiting } else { Hint::MuxDown }
            } else {
                Hint::NoCable
            }
        });
        Scan { devices, offline, hint }
    }
}

/// An Apple iOS device (vendor 05ac, product 12xx) present on the USB bus, whether or not usbmuxd sees it.
fn apple_on_usb() -> bool {
    std::fs::read_dir("/sys/bus/usb/devices").into_iter().flatten().flatten().any(|e| {
        let read = |f: &str| std::fs::read_to_string(e.path().join(f)).unwrap_or_default();
        read("idVendor").trim() == "05ac" && read("idProduct").starts_with("12")
    })
}

fn watch_wifi(found: Arc<Mutex<HashMap<String, IpAddr>>>) {
    let Ok(daemon) = mdns_sd::ServiceDaemon::new() else { return };
    let Ok(rx) = daemon.browse(MOBDEV2) else { return };
    std::thread::spawn(move || {
        let _daemon = daemon;
        let mut by_fullname: HashMap<String, String> = HashMap::new();
        while let Ok(ev) = rx.recv() {
            match ev {
                mdns_sd::ServiceEvent::ServiceResolved(s) => {
                    let Some(ip) = s.get_addresses_v4().into_iter().next() else { continue };
                    if let Some(udid) = match_pair(&s) {
                        by_fullname.insert(s.get_fullname().to_string(), udid.clone());
                        found.lock().unwrap().insert(udid, IpAddr::V4(ip));
                    }
                }
                mdns_sd::ServiceEvent::ServiceRemoved(_, full) => {
                    if let Some(udid) = by_fullname.remove(&full) {
                        found.lock().unwrap().remove(&udid);
                    }
                }
                _ => {}
            }
        }
    });
}

/// Older iOS names the service `<wifi-mac>@...`; iOS 26.4+ publishes `identifier` + `authTag` TXT values.
fn match_pair(s: &mdns_sd::ResolvedService) -> Option<String> {
    let instance = s.get_fullname().split('@').next().unwrap_or("").to_lowercase();
    let props = s.get_properties();
    let identifier = props.get_property_val("identifier").flatten();
    let tags: Vec<&[u8]> = props
        .iter()
        .filter(|p| p.key().starts_with("authTag"))
        .filter_map(|p| p.val())
        .collect();
    for entry in std::fs::read_dir(pairs_dir()).ok()?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "plist") {
            continue;
        }
        let Ok(pf) = PairingFile::read_from_file(&path) else { continue };
        let by_mac = !pf.wifi_mac_address.is_empty() && pf.wifi_mac_address.to_lowercase() == instance;
        let by_tag = identifier.is_some_and(|id| idevice::mdns::txt_record_matches(pf.host_id.as_bytes(), id, &tags));
        if by_mac || by_tag {
            return path.file_stem().map(|s| s.to_string_lossy().into_owned());
        }
    }
    None
}

/// A provider that may not have a pair record yet (enough for pre-trust lockdown queries).
fn raw_provider(udid: &str, t: &Transport) -> Result<Box<dyn IdeviceProvider>, IdeviceError> {
    Ok(match t {
        Transport::Usb(d) => Box::new(d.to_provider(UsbmuxdAddr::default(), LABEL)),
        Transport::Wifi(ip) => Box::new(TcpProvider {
            addr: *ip,
            scope_id: None,
            pairing_file: PairingFile::read_from_file(pair_path(udid, "plist"))?,
            label: LABEL.into(),
        }),
    })
}

async fn usb_pair_record(udid: &str) -> Option<PairingFile> {
    UsbmuxdConnection::default().await.ok()?.get_pair_record(udid).await.ok()
}

fn save_local(udid: &str, pf: PairingFile) {
    let _ = std::fs::create_dir_all(pairs_dir());
    if let Ok(bytes) = pf.serialize() {
        let _ = std::fs::write(pair_path(udid, "plist"), bytes);
    }
}

/// Removes a pair record the phone has rejected, so the UI offers Trust again.
async fn forget(udid: &str) {
    if let Ok(mut mux) = UsbmuxdConnection::default().await {
        let _ = mux.delete_pair_record(udid).await;
    }
    for ext in ["plist", "wifi"] {
        let _ = std::fs::remove_file(pair_path(udid, ext));
    }
}

async fn session(udid: &str, p: &dyn IdeviceProvider) -> Result<LockdownClient, IdeviceError> {
    let mut l = LockdownClient::connect(p).await?;
    match l.start_session(&p.get_pairing_file().await?).await {
        Err(IdeviceError::InvalidHostID) => {
            forget(udid).await;
            Err(IdeviceError::InvalidHostID)
        }
        r => r.map(|_| l),
    }
}

async fn str_val(l: &mut LockdownClient, key: &str) -> Option<String> {
    l.get_value(Some(key), None).await.ok()?.as_string().map(str::to_string)
}

async fn bool_val(l: &mut LockdownClient, key: &str, domain: &str) -> Option<bool> {
    l.get_value(Some(key), Some(domain)).await.ok()?.as_boolean()
}

async fn probe(udid: &str, t: &Transport) -> Info {
    let mut info = Info { name: known_name(udid).unwrap_or_else(|| "iPhone".into()), ..Default::default() };
    let Ok(p) = raw_provider(udid, t) else { return info };
    if let Ok(mut l) = LockdownClient::connect(p.as_ref()).await {
        if let Some(n) = str_val(&mut l, "DeviceName").await {
            info.name = n;
        }
        info.ios = str_val(&mut l, "ProductVersion").await.unwrap_or_default();
    }
    if let Transport::Usb(_) = t {
        match usb_pair_record(udid).await {
            Some(pf) => save_local(udid, pf),
            None => return info,
        }
    }
    let Ok(mut l) = session(udid, p.as_ref()).await else { return info };
    info.paired = true;
    info.encrypted = bool_val(&mut l, "WillEncrypt", "com.apple.mobile.backup").await;
    info.wifi_sync = bool_val(&mut l, "EnableWifiConnections", "com.apple.mobile.wireless_lockdown").await;
    info.find_my = bool_val(&mut l, "IsAssociated", "com.apple.fmip").await;
    let _ = std::fs::write(pair_path(udid, "name"), &info.name);
    if info.wifi_sync == Some(true) {
        let _ = std::fs::write(pair_path(udid, "wifi"), "");
    }
    info
}

/// Returns a provider for the device, pairing over USB first if the phone has never trusted this PC.
async fn provider(dev: &Device, log: Log<'_>) -> Result<Box<dyn IdeviceProvider>, String> {
    if let Transport::Usb(d) = &dev.transport
        && usb_pair_record(&d.udid).await.is_none()
    {
        log("Unlock the iPhone, tap Trust, and enter its passcode.".into());
        let mut mux = UsbmuxdConnection::default().await.map_err(friendly)?;
        let p = d.to_provider(UsbmuxdAddr::default(), LABEL);
        let mut lockdown = LockdownClient::connect(&p).await.map_err(friendly)?;
        let buid = mux.get_buid().await.map_err(friendly)?;
        let host_id = uuid::Uuid::new_v4().to_string().to_uppercase();
        let mut pf = lockdown.pair(host_id, buid, Some(LABEL)).await.map_err(friendly)?;
        pf.udid = Some(d.udid.clone());
        mux.save_pair_record(&d.udid, pf.clone().serialize().map_err(friendly)?)
            .await
            .map_err(friendly)?;
        save_local(&d.udid, pf);
    }
    raw_provider(&dev.udid, &dev.transport).map_err(friendly)
}

/// Keeps the phone's heartbeat answered while alive. Over Wi-Fi the phone kills every service
/// (BackupAgent2 exits with MBErrorDomain 100) unless the host holds a heartbeat session.
struct Beat(Option<tokio::task::JoinHandle<()>>);

impl Drop for Beat {
    fn drop(&mut self) {
        if let Some(h) = &self.0 {
            h.abort();
        }
    }
}

async fn beat(t: &Transport, p: &dyn IdeviceProvider) -> Result<Beat, String> {
    if let Transport::Usb(_) = t {
        return Ok(Beat(None));
    }
    let mut hb = HeartbeatClient::connect(p).await.map_err(friendly)?;
    Ok(Beat(Some(tokio::spawn(async move {
        let mut interval = 15;
        while let Ok(i) = hb.get_marco(interval + 5).await {
            interval = i;
            if hb.send_polo().await.is_err() {
                break;
            }
        }
    }))))
}

/// Starts mobilebackup2 with the pairing's escrow bag, which unlocks data protection so
/// encrypted restores can write protected files.
async fn mobilebackup(udid: &str, p: &dyn IdeviceProvider) -> Result<MobileBackup2Client, IdeviceError> {
    let pf = p.get_pairing_file().await?;
    let mut l = LockdownClient::connect(p).await?;
    let legacy = l.start_session(&pf).await?;
    let (port, ssl) = l.start_service_with_escrow(MobileBackup2Client::service_name(), pf.escrow_bag.clone()).await?;
    let mut d = p.connect(port).await?;
    if ssl {
        d.start_session(&pf, legacy).await?;
    }
    d.set_udid(udid);
    MobileBackup2Client::from_stream(d).await
}

pub async fn trust(dev: &Device, log: Log<'_>) -> Result<String, String> {
    let p = provider(dev, log).await?;
    session(&dev.udid, p.as_ref()).await.map_err(friendly)?;
    Ok(format!("{} trusts this computer.", dev.info.name))
}

/// Turns on Wi-Fi sync so the phone advertises itself on the local network.
pub async fn enable_wifi(dev: &Device, log: Log<'_>) -> Result<String, String> {
    let p = provider(dev, log).await?;
    let mut l = session(&dev.udid, p.as_ref()).await.map_err(friendly)?;
    l.set_value("EnableWifiConnections", true.into(), Some("com.apple.mobile.wireless_lockdown"))
        .await
        .map_err(friendly)?;
    let _ = std::fs::write(pair_path(&dev.udid, "wifi"), "");
    Ok("Wi-Fi sync is on. Unplug the cable; the iPhone appears as Wi-Fi when it's unlocked and on the same network.".into())
}

/// `new` set: turn encryption on (or change password if `old` is also set). `new` empty: turn it off.
pub async fn set_encryption(
    dev: &Device,
    root: &Path,
    old: Option<String>,
    new: Option<String>,
    log: Log<'_>,
) -> Result<String, String> {
    let p = provider(dev, log).await?;
    let want = new.is_some();
    log("If the iPhone asks for its passcode, enter it there.".into());
    let _beat = beat(&dev.transport, p.as_ref()).await?;
    let mut mb = mobilebackup(&dev.udid, p.as_ref()).await.map_err(friendly)?;
    let r = mb.change_password_from_path(root, old.as_deref(), new.as_deref(), &FsBackupDelegate).await;
    let _ = mb.disconnect().await;
    r.map_err(friendly)?;
    // The device doesn't reliably report a wrong password here, so check the result.
    let mut l = session(&dev.udid, p.as_ref()).await.map_err(friendly)?;
    match (bool_val(&mut l, "WillEncrypt", "com.apple.mobile.backup").await, want) {
        (Some(true), true) => Ok("Backups are now encrypted. Keep the password safe; restores need it.".into()),
        (Some(false), false) => Ok("Backup encryption is off.".into()),
        (_, true) => Err("Encryption wasn't turned on. Unlock the iPhone, enter its passcode when asked, and try again.".into()),
        (_, false) => Err("Wrong password, or the passcode wasn't entered on the iPhone. Encryption is still on.".into()),
    }
}

/// Host-side metadata iTunes/Finder expect next to the backup (the device doesn't send it).
fn write_info_plist(dir: &Path, udid: &str, info: &plist::Dictionary) -> Result<(), String> {
    let get = |k: &str| info.get(k).cloned().unwrap_or_else(|| plist::Value::String(String::new()));
    let mut d = plist::Dictionary::new();
    d.insert("Device Name".into(), get("DeviceName"));
    d.insert("Display Name".into(), get("DeviceName"));
    d.insert("Build Version".into(), get("BuildVersion"));
    d.insert("Product Type".into(), get("ProductType"));
    d.insert("Product Version".into(), get("ProductVersion"));
    d.insert("Serial Number".into(), get("SerialNumber"));
    for (ours, theirs) in [
        ("IMEI", "InternationalMobileEquipmentIdentity"),
        ("Phone Number", "PhoneNumber"),
        ("ICCID", "IntegratedCircuitCardIdentity"),
    ] {
        if let Some(v) = info.get(theirs) {
            d.insert(ours.into(), v.clone());
        }
    }
    d.insert("GUID".into(), uuid::Uuid::new_v4().simple().to_string().to_uppercase().into());
    d.insert("Last Backup Date".into(), plist::Value::Date(std::time::SystemTime::now().into()));
    d.insert("Target Identifier".into(), udid.into());
    d.insert("Target Type".into(), "Device".into());
    d.insert("Unique Identifier".into(), udid.to_uppercase().into());
    d.insert("iTunes Version".into(), "12.13".into());
    std::fs::create_dir_all(dir).map_err(io_err)?;
    plist::to_file_xml(dir.join("Info.plist"), &d).map_err(io_err)
}

/// Forwards progress to the UI while delegating all file I/O to the stock filesystem delegate.
struct Progress<'a>(Prog<'a>);

macro_rules! fwd {
    ($($name:ident($($a:ident: $t:ty),*) -> $r:ty;)*) => {
        $(fn $name<'a>(&'a self, $($a: &'a $t),*) -> $r { FsBackupDelegate.$name($($a),*) })*
    };
}

impl BackupDelegate for Progress<'_> {
    fn get_free_disk_space(&self, path: &Path) -> u64 {
        FsBackupDelegate.get_free_disk_space(path)
    }
    fwd! {
        open_file_read(path: Path) -> std::pin::Pin<Box<dyn Future<Output = Result<Box<dyn std::io::Read + Send>, IdeviceError>> + Send + 'a>>;
        create_file_write(path: Path) -> std::pin::Pin<Box<dyn Future<Output = Result<Box<dyn std::io::Write + Send>, IdeviceError>> + Send + 'a>>;
        create_dir_all(path: Path) -> std::pin::Pin<Box<dyn Future<Output = Result<(), IdeviceError>> + Send + 'a>>;
        remove(path: Path) -> std::pin::Pin<Box<dyn Future<Output = Result<(), IdeviceError>> + Send + 'a>>;
        rename(from: Path, to: Path) -> std::pin::Pin<Box<dyn Future<Output = Result<(), IdeviceError>> + Send + 'a>>;
        copy(src: Path, dst: Path) -> std::pin::Pin<Box<dyn Future<Output = Result<(), IdeviceError>> + Send + 'a>>;
        exists(path: Path) -> std::pin::Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
        is_dir(path: Path) -> std::pin::Pin<Box<dyn Future<Output = bool> + Send + 'a>>;
        list_dir(path: Path) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<idevice::mobilebackup2::DirEntryInfo>, IdeviceError>> + Send + 'a>>;
    }
    fn on_progress(&self, p: BackupProgress) {
        (self.0)(p);
    }
}

/// Turns the device's final status dictionary into a result.
fn finish(res: Result<Option<plist::Dictionary>, IdeviceError>, op: &str) -> Result<(), String> {
    // A disconnect without a final status means the phone gave up mid-way.
    let Some(res) = res.map_err(friendly)? else {
        return Err(format!("The iPhone ended the {op} early. {}", lost()));
    };
    let res = Some(res);
    let get = |k: &str| res.as_ref().and_then(|d| d.get(k)).cloned();
    let code = get("ErrorCode").and_then(|v| v.as_signed_integer()).unwrap_or(0);
    if code != 0 {
        let desc = get("ErrorDescription").and_then(|v| v.into_string()).unwrap_or_else(|| "no reason given".into());
        return Err(format!("The iPhone stopped the {op}: {desc} (error {code})."));
    }
    Ok(())
}

pub async fn backup(dev: &Device, root: &Path, log: Log<'_>, progress: Prog<'_>) -> Result<String, String> {
    let p = provider(dev, log).await?;
    let mut l = session(&dev.udid, p.as_ref()).await.map_err(friendly)?;
    let info = l.get_value(None, None).await.ok().and_then(|v| v.into_dictionary()).unwrap_or_default();
    log("Backing up. Keep the iPhone unlocked and connected.".into());
    let _beat = beat(&dev.transport, p.as_ref()).await?;
    let mut mb = mobilebackup(&dev.udid, p.as_ref()).await.map_err(friendly)?;
    let res = mb.backup_from_path(root, Some(&dev.udid), None, &Progress(progress)).await;
    let _ = mb.disconnect().await;
    finish(res, "backup").map_err(|e| {
        let free = FsBackupDelegate.get_free_disk_space(root) as f64 / 1e9;
        format!("{e} The backup folder's drive has {free:.1} GB free; a full backup needs about as much as the iPhone's used storage.")
    })?;
    write_info_plist(&root.join(&dev.udid), &dev.udid, &info)?;
    Ok("Backup complete.".into())
}

pub struct RestoreChoice {
    pub source_udid: String,
    pub system: bool,
    pub settings: bool,
    pub remove: bool,
    pub reboot: bool,
    pub password: Option<String>,
}

pub async fn restore(dev: &Device, root: &Path, c: &RestoreChoice, log: Log<'_>, progress: Prog<'_>) -> Result<String, String> {
    let p = provider(dev, log).await?;
    let mut l = session(&dev.udid, p.as_ref()).await.map_err(friendly)?;
    if bool_val(&mut l, "IsAssociated", "com.apple.fmip").await == Some(true) {
        return Err(find_my_text());
    }
    log("Restoring. Don't unplug the iPhone or close this window.".into());
    let mut opts = RestoreOptions::new()
        .with_copy(false)
        .with_system_files(c.system)
        .with_preserve_settings(!c.settings)
        .with_remove_items_not_restored(c.remove)
        .with_reboot(c.reboot);
    if let Some(pw) = &c.password {
        opts = opts.with_password(pw.clone());
    }
    let _beat = beat(&dev.transport, p.as_ref()).await?;
    let mut mb = mobilebackup(&dev.udid, p.as_ref()).await.map_err(friendly)?;
    let res = mb.restore_from_path(root, Some(&c.source_udid), Some(opts), &Progress(progress)).await;
    let _ = mb.disconnect().await;
    finish(res, "restore")?;
    Ok(if c.reboot { "Restore complete. The iPhone is restarting." } else { "Restore complete." }.into())
}

pub fn find_my_text() -> String {
    "Find My is on. On the iPhone open Settings → your name → Find My → Find My iPhone, turn it off, then try again.".into()
}

fn dir_size(p: &Path) -> u64 {
    std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| match e.file_type() {
        Ok(t) if t.is_dir() => dir_size(&e.path()),
        Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
        Err(_) => 0,
    }).sum()
}

/// Backups in `root`, newest first: one sub-folder per source device, named by its UDID.
pub fn list_backups(root: &Path) -> Vec<BackupEntry> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let dir = entry.path();
        let Ok(manifest) = plist::Value::from_file(dir.join("Manifest.plist")) else { continue };
        let info = plist::Value::from_file(dir.join("Info.plist")).ok();
        let info = info.as_ref().and_then(|v| v.as_dictionary());
        let s = |k: &str| info.and_then(|d| d.get(k)).and_then(|v| v.as_string()).map(str::to_string);
        let date = info
            .and_then(|d| d.get("Last Backup Date"))
            .and_then(|v| v.as_date())
            .map(|d| d.to_xml_format().replace('T', " ").trim_end_matches('Z').to_string())
            .unwrap_or_default();
        let udid = entry.file_name().to_string_lossy().into_owned();
        out.push(BackupEntry {
            name: s("Device Name").unwrap_or_else(|| udid.clone()),
            ios: s("Product Version").unwrap_or_default(),
            size: dir_size(&dir),
            udid,
            date,
            encrypted: manifest
                .as_dictionary()
                .and_then(|d| d.get("IsEncrypted"))
                .and_then(|v| v.as_boolean())
                .unwrap_or(false),
        });
    }
    out.sort_by(|a, b| b.date.cmp(&a.date));
    out
}

/// True if iOS version `a` is older than `b` ("17.5.1" < "18.0").
pub fn older(a: &str, b: &str) -> bool {
    let parse = |s: &str| s.split('.').map(|n| n.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    !a.is_empty() && !b.is_empty() && parse(a) < parse(b)
}

/// Installs usbmuxd with this distro's package manager, asking for the password through pkexec.
pub async fn install_usbmuxd() -> Result<String, String> {
    const MANAGERS: [&[&str]; 4] = [
        &["pacman", "-S", "--noconfirm", "--needed", "usbmuxd"],
        &["apt-get", "install", "-y", "usbmuxd"],
        &["dnf", "install", "-y", "usbmuxd"],
        &["zypper", "--non-interactive", "install", "usbmuxd"],
    ];
    let cmd = MANAGERS
        .into_iter()
        .find(|c| Path::new("/usr/bin").join(c[0]).exists())
        .ok_or("No supported package manager found. Install the usbmuxd package yourself.")?;
    let out = tokio::task::spawn_blocking(move || std::process::Command::new("pkexec").args(cmd).output())
        .await
        .map_err(io_err)?
        .map_err(|e| format!("Couldn't run pkexec ({e})."))?;
    match out.status.code() {
        Some(0) => Ok("usbmuxd is installed. Unplug the iPhone and plug it back in.".into()),
        Some(126) | Some(127) => Err("The password dialog was cancelled, so nothing was installed.".into()),
        _ => Err(format!(
            "Installing usbmuxd failed: {}",
            String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("unknown error")
        )),
    }
}
