# Phone Backup

Back up and restore an iPhone from Linux, over USB or Wi-Fi. Backups use the same format as iTunes/Finder.

> **Alpha.** USB and Wi-Fi backups work. **Restore has not been tested yet.** Don't rely on it for a phone whose only copy of its data is the backup.

Not affiliated with Apple. iPhone is a trademark of Apple Inc.

## Features

- Full and incremental backups, over USB or Wi-Fi
- Restore any backup onto any iPhone, including a new one
- Backup encryption on/off, so backups include saved passwords and Health data
- Checks before restoring: Find My, iOS version, backup password
- Progress with size, speed and time left; cancel (except mid-restore)

## Requirements

- Linux
- `usbmuxd` for USB. If it's missing, Phone Backup offers to install it.
- Rust 1.95+ to build

## Build and run

```sh
cargo build --release
./target/release/phonebackup
```

## Usage

1. Plug the iPhone in over USB, unlock it, click **Trust this computer**, and tap **Trust** on the phone.
2. Click **Back up now**. Backups go to `~/iPhoneBackups` by default.
3. To restore, pick a backup and click **Restore…**. Turn off Find My on the phone first.

### Wi-Fi

Plug in once and click **Enable Wi-Fi sync**. After that the iPhone shows up as Wi-Fi whenever it's unlocked and on the same network, with the cable unplugged. Discovery uses mDNS, so the firewall must allow UDP 5353. Wi-Fi is slower than USB, so do the first full backup with the cable.

## How it works

Phone Backup is built on the [`idevice`](https://crates.io/crates/idevice) crate: lockdown, pairing and mobilebackup2. It doesn't use libimobiledevice. Over Wi-Fi it finds the phone through `_apple-mobdev2._tcp` and connects directly; it doesn't need netmuxd. It also keeps a heartbeat session open, because without one iOS ends every service connection on Wi-Fi.

Pairing records are stored in `~/.local/share/phonebackup/pairs`.

## License

MIT
