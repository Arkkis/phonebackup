mod device;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use device::{BackupEntry, Device, Hint, RestoreChoice, Scan, Scanner};
use eframe::egui::{self, Color32, RichText};
use idevice::mobilebackup2::BackupProgress;

type Log = Box<dyn Fn(String) + Send + Sync>;
type Prog = Box<dyn Fn(BackupProgress) + Send + Sync>;

const OK: Color32 = Color32::from_rgb(90, 180, 110);

enum Msg {
    Scan(Scan),
    Log(String),
    Progress(BackupProgress),
    Done(Result<String, String>),
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Backup,
    Restore,
    Other,
}

struct Job {
    kind: Kind,
    title: String,
    handle: tokio::task::JoinHandle<()>,
    started: Instant,
    progress: Option<BackupProgress>,
}

struct Encrypt {
    on: bool,
    pw: String,
    pw2: String,
}

struct App {
    rt: tokio::runtime::Runtime,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    scanner: Arc<Scanner>,
    scanning: bool,
    last_scan: Option<Instant>,
    scan: Scan,
    selected: Option<String>,
    root: PathBuf,
    backups: Vec<BackupEntry>,
    source: Option<String>,
    system: bool,
    settings: bool,
    remove: bool,
    reboot: bool,
    password: String,
    job: Option<Job>,
    result: Option<Result<String, String>>,
    log: Vec<String>,
    confirm_restore: bool,
    encrypt: Option<Encrypt>,
    confirm_quit: bool,
    quit_ok: bool,
}

impl App {
    fn new() -> Self {
        let (tx, rx) = channel();
        let root = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("iPhoneBackups");
        Self {
            rt: tokio::runtime::Runtime::new().expect("tokio runtime"),
            tx,
            rx,
            scanner: Scanner::new(),
            scanning: false,
            last_scan: None,
            scan: Scan { devices: Vec::new(), offline: Vec::new(), hint: None },
            selected: None,
            backups: device::list_backups(&root),
            root,
            source: None,
            system: true,
            settings: true,
            remove: false,
            reboot: true,
            password: String::new(),
            job: None,
            result: None,
            log: Vec::new(),
            confirm_restore: false,
            encrypt: None,
            confirm_quit: false,
            quit_ok: false,
        }
    }

    fn device(&self) -> Option<Device> {
        self.scan.devices.iter().find(|d| Some(&d.udid) == self.selected.as_ref()).cloned()
    }

    fn chosen(&self) -> Option<&BackupEntry> {
        self.backups.iter().find(|b| Some(&b.udid) == self.source.as_ref())
    }

    fn last_backup(&self, udid: &str) -> Option<&BackupEntry> {
        self.backups.iter().find(|b| b.udid == udid)
    }

    fn rescan(&mut self, ctx: &egui::Context) {
        if self.scanning || self.job.is_some() || self.last_scan.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
            return;
        }
        self.scanning = true;
        let (tx, scanner, ctx) = (self.tx.clone(), self.scanner.clone(), ctx.clone());
        self.rt.spawn(async move {
            let _ = tx.send(Msg::Scan(scanner.scan().await));
            ctx.request_repaint();
        });
    }

    /// Runs one device job in the background, streaming log lines and progress back to the UI.
    fn run<F, Fut>(&mut self, ctx: &egui::Context, kind: Kind, title: &str, job: F)
    where
        F: FnOnce(Log, Prog) -> Fut,
        Fut: Future<Output = Result<String, String>> + Send + 'static,
    {
        let (t1, c1) = (self.tx.clone(), ctx.clone());
        let log: Log = Box::new(move |s| {
            let _ = t1.send(Msg::Log(s));
            c1.request_repaint();
        });
        let (t2, c2) = (self.tx.clone(), ctx.clone());
        let prog: Prog = Box::new(move |p| {
            let _ = t2.send(Msg::Progress(p));
            c2.request_repaint();
        });
        let fut = job(log, prog);
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        let handle = self.rt.spawn(async move {
            let _ = tx.send(Msg::Done(fut.await));
            ctx.request_repaint();
        });
        self.result = None;
        self.log.push(format!("— {title} —"));
        self.job = Some(Job { kind, title: title.into(), handle, started: Instant::now(), progress: None });
    }

    fn end_job(&mut self, r: Result<String, String>) {
        self.job = None;
        self.log.push(match &r {
            Ok(s) => s.clone(),
            Err(e) => format!("Failed: {e}"),
        });
        self.result = Some(r);
        self.scanner.refresh();
        self.last_scan = None;
        self.backups = device::list_backups(&self.root);
    }

    fn pump(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Scan(s) => {
                    self.scanning = false;
                    self.last_scan = Some(Instant::now());
                    if self.selected.as_ref().is_none_or(|u| !s.devices.iter().any(|d| &d.udid == u)) {
                        self.selected = s.devices.first().map(|d| d.udid.clone());
                    }
                    self.scan = s;
                }
                Msg::Log(s) => self.log.push(s),
                Msg::Progress(p) => {
                    if let Some(j) = &mut self.job {
                        j.progress = Some(p);
                    }
                }
                Msg::Done(r) => self.end_job(r),
            }
        }
    }

    fn device_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        card(ui, "iPhone", |ui| {
            if self.scan.devices.is_empty() {
                ui.label(RichText::new(hint_text(self.scan.hint)).color(ui.visuals().warn_fg_color));
                if self.scan.hint == Some(Hint::NoUsbmuxd) && ui.button("Install usbmuxd").clicked() {
                    self.run(ctx, Kind::Other, "Installing usbmuxd", |_, _| device::install_usbmuxd());
                }
            }
            let many = self.scan.devices.len() > 1;
            for d in self.scan.devices.clone() {
                let via = if d.is_usb() { "USB" } else { "Wi-Fi" };
                let ios = if d.info.ios.is_empty() { String::new() } else { format!("iOS {} · ", d.info.ios) };
                let last = match self.last_backup(&d.udid) {
                    Some(b) => format!("last backup {}", b.date),
                    None => "never backed up here".into(),
                };
                ui.horizontal(|ui| {
                    if many {
                        ui.radio_value(&mut self.selected, Some(d.udid.clone()), "");
                    }
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&d.info.name).size(18.0).strong());
                        ui.label(RichText::new(format!("{ios}{via} · {last}")).weak());
                    });
                });
            }

            if let Some(d) = self.device() {
                ui.add_space(4.0);
                if !d.info.paired {
                    ui.label(RichText::new("This computer isn't trusted yet. Click below, then tap Trust on the iPhone and enter its passcode.").color(ui.visuals().warn_fg_color));
                    if ui.button("Trust this computer").clicked() {
                        self.run(ctx, Kind::Other, "Pairing", move |log, _| async move { device::trust(&d, &*log).await });
                    }
                } else if !d.is_usb() {
                    ui.label(RichText::new("📶 Connected over Wi-Fi. Wi-Fi is slower than USB; the first full backup is quicker with the cable.").weak());
                } else if d.info.wifi_sync == Some(true) {
                    ui.label(RichText::new("📶 Wi-Fi sync is on. This iPhone also appears here without the cable when it's unlocked and on the same network.").weak());
                } else {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("Enable Wi-Fi sync").clicked() {
                            self.run(ctx, Kind::Other, "Enabling Wi-Fi sync", move |log, _| async move { device::enable_wifi(&d, &*log).await });
                        }
                        ui.label(RichText::new("Lets you back up later without the cable.").weak());
                    });
                }
            }

            for name in &self.scan.offline {
                ui.label(RichText::new(format!(
                    "📶 {name} isn't visible on Wi-Fi. Unlock it and check it's on the same network as this PC. \
                     If it never shows up, allow mDNS (UDP 5353) in your firewall."
                )).weak());
            }
        });
    }

    fn backup_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let dev = self.device().filter(|d| d.info.paired);
        card(ui, "Back up", |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Save to");
                ui.monospace(self.root.display().to_string());
                if ui.small_button("Change…").clicked()
                    && let Some(p) = rfd::FileDialog::new().set_directory(&self.root).pick_folder()
                {
                    self.root = p;
                    self.source = None;
                    self.backups = device::list_backups(&self.root);
                }
            });
            if let Some(d) = &dev {
                match d.info.encrypted {
                    Some(false) => {
                        ui.label(RichText::new("⚠ Backups aren't encrypted, so saved passwords, Health and Wi-Fi data are left out.").color(ui.visuals().warn_fg_color));
                        if ui.button("Encrypt backups…").clicked() {
                            self.encrypt = Some(Encrypt { on: true, pw: String::new(), pw2: String::new() });
                        }
                    }
                    Some(true) => {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new("🔒 Encrypted: backups include saved passwords and Health data.").color(OK));
                            if ui.small_button("Turn off…").clicked() {
                                self.encrypt = Some(Encrypt { on: false, pw: String::new(), pw2: String::new() });
                            }
                        });
                    }
                    None => {}
                }
            }
            ui.add_space(4.0);
            if ui.add_enabled(dev.is_some(), egui::Button::new(RichText::new("Back up now").size(16.0)).min_size(egui::vec2(140.0, 32.0))).clicked()
                && let Some(d) = dev
            {
                let root = self.root.clone();
                self.run(ctx, Kind::Backup, &format!("Backing up {}", d.info.name), move |log, prog| async move {
                    std::fs::create_dir_all(&root).map_err(|e| format!("Can't create the backup folder: {e}"))?;
                    device::backup(&d, &root, &*log, &*prog).await
                });
            }
        });
    }

    fn restore_card(&mut self, ui: &mut egui::Ui) {
        let dev = self.device().filter(|d| d.info.paired);
        card(ui, "Restore", |ui| {
            if self.backups.is_empty() {
                ui.label(RichText::new("No backups in this folder yet.").weak());
                return;
            }
            ui.label("Choose a backup. Backups from any iPhone can be restored onto this one.");
            for b in &self.backups {
                let lock = if b.encrypted { " 🔒" } else { "" };
                let ios = if b.ios.is_empty() { String::new() } else { format!(" · iOS {}", b.ios) };
                ui.radio_value(&mut self.source, Some(b.udid.clone()), format!("{}{lock} — {}{ios} · {}", b.name, b.date, size(b.size)));
            }
            egui::CollapsingHeader::new("Options").show(ui, |ui| {
                ui.checkbox(&mut self.system, "Restore system files").on_hover_text("Needed for a complete restore, as iTunes/Finder does.");
                ui.checkbox(&mut self.settings, "Restore settings").on_hover_text("Off keeps the iPhone's current settings.");
                ui.checkbox(&mut self.remove, "Remove items not in the backup").on_hover_text("Deletes anything on the iPhone the backup doesn't have.");
                ui.checkbox(&mut self.reboot, "Restart the iPhone when done");
            });
            if self.chosen().is_some_and(|b| b.encrypted) {
                ui.horizontal(|ui| {
                    ui.label("Backup password");
                    ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                });
            }
            ui.add_space(4.0);
            if ui.add_enabled(dev.is_some() && self.chosen().is_some(), egui::Button::new("Restore…").min_size(egui::vec2(140.0, 32.0))).clicked() {
                self.confirm_restore = true;
            }
        });
    }

    fn status(&mut self, ui: &mut egui::Ui) {
        if let Some(j) = &self.job {
            let mut cancel = false;
            card(ui, &j.title, |ui| {
                match j.progress.filter(|p| p.overall_progress >= 0.0) {
                    Some(p) => {
                        ui.add(egui::ProgressBar::new(p.overall_progress as f32 / 100.0).show_percentage());
                        ui.label(RichText::new(progress_text(&p, j.started.elapsed())).weak());
                    }
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Working…");
                        });
                    }
                }
                if let Some(last) = self.log.last() {
                    ui.label(last);
                }
                if j.kind == Kind::Restore {
                    ui.label(RichText::new("Don't unplug the iPhone or close this window.").color(ui.visuals().warn_fg_color));
                } else {
                    cancel = ui.button("Cancel").clicked();
                }
            });
            if cancel {
                let kind = j.kind;
                j.handle.abort();
                self.end_job(Err(if kind == Kind::Backup {
                    "Backup cancelled. The iPhone wasn't changed.".into()
                } else {
                    "Cancelled.".into()
                }));
            }
        } else if let Some(r) = &self.result {
            let (text, color) = match r {
                Ok(s) => (format!("✔ {s}"), OK),
                Err(e) => (format!("✖ {e}"), ui.visuals().error_fg_color),
            };
            card(ui, "", |ui| {
                ui.label(RichText::new(text).color(color).size(15.0));
            });
        }
        if !self.log.is_empty() {
            egui::CollapsingHeader::new("Details").show(ui, |ui| {
                for l in &self.log {
                    ui.label(RichText::new(l).monospace().weak());
                }
            });
        }
    }

    fn restore_dialog(&mut self, ctx: &egui::Context) {
        let (Some(dev), Some(b)) = (self.device(), self.chosen()) else {
            self.confirm_restore = false;
            return;
        };
        let (source, b_name, b_date, b_ios, b_enc) = (b.udid.clone(), b.name.clone(), b.date.clone(), b.ios.clone(), b.encrypted);
        let mut go = false;
        let mut open = true;
        egui::Window::new("Restore this backup?").collapsible(false).resizable(false).open(&mut open).show(ctx, |ui| {
            ui.label(format!("“{b_name}” from {b_date} → {}", dev.info.name));
            ui.label(RichText::new(format!("Everything on {} is replaced with the backup.", dev.info.name)).strong());
            ui.add_space(6.0);
            let err = ui.visuals().error_fg_color;
            let mut blocked = false;
            if dev.info.find_my == Some(true) {
                ui.label(RichText::new(format!("✖ {}", device::find_my_text())).color(err));
                blocked = true;
            } else if dev.info.find_my.is_none() {
                ui.label("• Make sure Find My is off on the iPhone (Settings → your name → Find My).");
            }
            if device::older(&dev.info.ios, &b_ios) {
                ui.label(RichText::new(format!(
                    "✖ This backup is from iOS {b_ios}, but the iPhone has iOS {}. Update the iPhone first: Settings → General → Software Update.",
                    dev.info.ios
                )).color(err));
                blocked = true;
            }
            if b_enc && self.password.is_empty() {
                ui.label(RichText::new("✖ This backup is encrypted. Enter its password in the Restore section.").color(err));
                blocked = true;
            }
            ui.label("• Keep the iPhone plugged in and this window open until it finishes.");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                go = ui.add_enabled(!blocked, egui::Button::new("Restore")).clicked();
                if ui.button("Cancel").clicked() {
                    self.confirm_restore = false;
                }
            });
        });
        if !open {
            self.confirm_restore = false;
        }
        if go {
            self.confirm_restore = false;
            let c = RestoreChoice {
                source_udid: source,
                system: self.system,
                settings: self.settings,
                remove: self.remove,
                reboot: self.reboot,
                password: (b_enc && !self.password.is_empty()).then(|| self.password.clone()),
            };
            let root = self.root.clone();
            self.run(ctx, Kind::Restore, &format!("Restoring {}", dev.info.name), move |log, prog| async move {
                device::restore(&dev, &root, &c, &*log, &*prog).await
            });
        }
    }

    fn encrypt_dialog(&mut self, ctx: &egui::Context) {
        let Some(dev) = self.device() else {
            self.encrypt = None;
            return;
        };
        let Some(e) = &mut self.encrypt else { return };
        let mut submit = None;
        let mut close = false;
        let title = if e.on { "Encrypt backups" } else { "Turn off encryption" };
        egui::Window::new(title).collapsible(false).resizable(false).show(ctx, |ui| {
            let problem = if e.on {
                ui.label("Choose a password for this iPhone's backups.");
                ui.add(egui::TextEdit::singleline(&mut e.pw).password(true).hint_text("Password"));
                ui.add(egui::TextEdit::singleline(&mut e.pw2).password(true).hint_text("Repeat password"));
                ui.label(RichText::new("Write it down. Without it, these backups can't be restored.").color(ui.visuals().warn_fg_color));
                if e.pw.is_empty() {
                    Some("")
                } else if e.pw != e.pw2 {
                    Some("The passwords don't match.")
                } else {
                    None
                }
            } else {
                ui.label("Enter the current backup password.");
                ui.add(egui::TextEdit::singleline(&mut e.pw).password(true).hint_text("Password"));
                ui.label(RichText::new("Future backups will leave out saved passwords and Health data.").weak());
                e.pw.is_empty().then_some("")
            };
            if let Some(p) = problem.filter(|p| !p.is_empty()) {
                ui.label(RichText::new(p).color(ui.visuals().error_fg_color));
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(problem.is_none(), egui::Button::new(if e.on { "Encrypt" } else { "Turn off" })).clicked() {
                    submit = Some((e.on, e.pw.clone()));
                }
                close = ui.button("Cancel").clicked();
            });
        });
        if close {
            self.encrypt = None;
        }
        if let Some((on, pw)) = submit {
            self.encrypt = None;
            let root = self.root.clone();
            let (old, new) = if on { (None, Some(pw)) } else { (Some(pw), None) };
            let title = if on { "Turning on encryption" } else { "Turning off encryption" };
            self.run(ctx, Kind::Other, title, move |log, _| async move {
                std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
                device::set_encryption(&dev, &root, old, new, &*log).await
            });
        }
    }

    fn quit_dialog(&mut self, ctx: &egui::Context) {
        let restoring = self.job.as_ref().is_some_and(|j| j.kind == Kind::Restore);
        egui::Window::new("Quit Phone Backup?").collapsible(false).resizable(false).show(ctx, |ui| {
            ui.label(if restoring {
                "A restore is running. Quitting now can leave the iPhone half-restored until you restore it again."
            } else {
                "A job is still running. Quitting stops it."
            });
            ui.horizontal(|ui| {
                if ui.button("Keep running").clicked() {
                    self.confirm_quit = false;
                }
                if ui.button("Quit anyway").clicked() {
                    self.quit_ok = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

fn card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).inner_margin(12.0).corner_radius(8.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if !title.is_empty() {
            ui.label(RichText::new(title).size(13.0).weak().strong());
        }
        add(ui);
    });
    ui.add_space(8.0);
}

fn hint_text(h: Option<Hint>) -> &'static str {
    match h {
        Some(Hint::NoUsbmuxd) => "usbmuxd isn't installed, so iPhones on USB can't be seen. Install the usbmuxd package, then plug the iPhone in again.",
        Some(Hint::MuxDown) => "An iPhone is plugged in, but usbmuxd isn't running. Unplug and replug the cable, or run: systemctl start usbmuxd",
        Some(Hint::Waiting) => "An iPhone is plugged in but not answering yet. Unlock it and wait a few seconds.",
        Some(Hint::NoCable) | None => "No iPhone found. Plug it in with a data cable (some cables only charge) and unlock it.",
    }
}

fn size(b: u64) -> String {
    let b = b as f64;
    match b {
        b if b >= 1e9 => format!("{:.1} GB", b / 1e9),
        b if b >= 1e6 => format!("{:.0} MB", b / 1e6),
        b => format!("{:.0} KB", b / 1e3),
    }
}

fn progress_text(p: &BackupProgress, elapsed: Duration) -> String {
    let done = p.session_bytes_done;
    let mut parts = vec![if p.session_bytes_total > done {
        format!("{} of about {}", size(done), size(p.session_bytes_total))
    } else {
        size(done)
    }];
    let secs = elapsed.as_secs_f64();
    if secs > 3.0 && done > 0 {
        let speed = done as f64 / secs;
        parts.push(format!("{}/s", size(speed as u64)));
        if p.session_bytes_total > done {
            let left = ((p.session_bytes_total - done) as f64 / speed) as u64;
            parts.push(match left {
                0..60 => "less than a minute left".into(),
                60..3600 => format!("about {} min left", left / 60),
                _ => format!("about {} h {} min left", left / 3600, left % 3600 / 60),
            });
        }
    }
    parts.join(" · ")
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        self.pump();
        self.rescan(ctx);
        ctx.request_repaint_after(Duration::from_secs(2));

        if ctx.input(|i| i.viewport().close_requested()) && self.job.is_some() && !self.quit_ok {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_quit = true;
        }

        let modal = self.confirm_restore || self.encrypt.is_some() || self.confirm_quit;
        egui::CentralPanel::default().show(root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_enabled_ui(self.job.is_none() && !modal, |ui| {
                    self.device_card(ui, ctx);
                    self.backup_card(ui, ctx);
                    self.restore_card(ui);
                });
                self.status(ui);
            });
        });

        if self.confirm_restore {
            self.restore_dialog(ctx);
        }
        if self.encrypt.is_some() {
            self.encrypt_dialog(ctx);
        }
        if self.confirm_quit {
            self.quit_dialog(ctx);
        }
    }
}

fn main() -> eframe::Result {
    eframe::run_native(
        "Phone Backup",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([560.0, 720.0]).with_app_id("phonebackup"),
            ..Default::default()
        },
        Box::new(|_| Ok(Box::new(App::new()))),
    )
}
