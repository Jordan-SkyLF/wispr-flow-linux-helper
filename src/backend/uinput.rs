//! In-process uinput virtual keyboard (Wayland injection primitive).
//!
//! On Wayland there is no portable X11-style synthetic-input API: XTEST events
//! don't reach native Wayland surfaces. The reliable, compositor-agnostic path
//! is to create a *real* virtual input device via `/dev/uinput` and write kernel
//! key events to it — the compositor sees them as ordinary hardware input and
//! routes them to the focused surface like any keyboard.
//!
//! This is what `ydotool` does, but in-process: we don't shell out to `ydotool`
//! (which needs its `ydotoold` daemon running). We just need write access to
//! `/dev/uinput` (typically granted to the active-session user via a logind
//! `uaccess` udev rule / ACL; otherwise the `uinput` group or root).
//!
//! Codes written here are Linux evdev `KEY_*` codes (see `keymap::vk_to_evdev`),
//! NOT X11 keysyms.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

use super::Result;
use crate::keymap;

// --- evdev event types (<linux/input-event-codes.h>) ---
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const SYN_REPORT: u16 = 0x00;

// --- uinput ioctls (<linux/uinput.h>), x86_64 encodings ---
//   UI_DEV_CREATE   = _IO('U', 1)            = 0x5501
//   UI_DEV_DESTROY  = _IO('U', 2)            = 0x5502
//   UI_SET_EVBIT    = _IOW('U', 100, int)    = 0x40045564
//   UI_SET_KEYBIT   = _IOW('U', 101, int)    = 0x40045565
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_SET_EVBIT: libc::c_ulong = 0x40045564;
const UI_SET_KEYBIT: libc::c_ulong = 0x40045565;

const BUS_USB: u16 = 0x03;
/// We enable the full standard key range so any mapped VK can be injected.
const KEY_MAX: u16 = 0x2ff;

pub struct UInput {
    // Closing this fd destroys the virtual device even after a failed write.
    file: Option<File>,
}

impl UInput {
    /// Probe whether `/dev/uinput` is openable for writing without creating a
    /// device (used by backend detection so we can fall back gracefully).
    pub fn available() -> bool {
        OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")
            .is_ok()
    }

    /// Create the virtual keyboard. Must be kept alive for the process lifetime;
    /// dropping it destroys the device.
    pub fn create() -> Result<UInput> {
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")
            .map_err(|e| format!("open /dev/uinput: {e} (need write access — logind uaccess ACL, `uinput` group, or root)"))?;
        let fd = file.as_raw_fd();

        // Declare the event types and key range this device emits.
        ioctl_set(fd, UI_SET_EVBIT, EV_KEY as libc::c_int)?;
        ioctl_set(fd, UI_SET_EVBIT, EV_SYN as libc::c_int)?;
        for code in 1..=KEY_MAX {
            // Best-effort: a few codes in the range are gaps; ignore EINVAL.
            unsafe { libc::ioctl(fd, UI_SET_KEYBIT, code as libc::c_int) };
        }

        // Legacy device-setup path (write a uinput_user_dev, then UI_DEV_CREATE):
        // widely supported and avoids the newer UI_DEV_SETUP/abs_setup structs.
        let mut dev: libc::uinput_user_dev = unsafe { std::mem::zeroed() };
        let name = b"Wispr Flow Linux Helper";
        for (i, &b) in name.iter().enumerate() {
            dev.name[i] = b as libc::c_char;
        }
        dev.id.bustype = BUS_USB;
        dev.id.vendor = 0x1234;
        dev.id.product = 0x5678;
        dev.id.version = 1;

        let bytes = unsafe {
            std::slice::from_raw_parts(
                &dev as *const _ as *const u8,
                std::mem::size_of::<libc::uinput_user_dev>(),
            )
        };
        (&file)
            .write_all(bytes)
            .map_err(|e| format!("write uinput_user_dev: {e}"))?;

        if unsafe { libc::ioctl(fd, UI_DEV_CREATE) } < 0 {
            return Err(format!(
                "UI_DEV_CREATE: {}",
                std::io::Error::last_os_error()
            ));
        }

        // The compositor needs a moment to enumerate the new device before it
        // will route events from it; injecting too early drops the first keys.
        std::thread::sleep(std::time::Duration::from_millis(200));

        Ok(UInput { file: Some(file) })
    }

    fn emit(&mut self, type_: u16, code: u16, value: i32) -> Result<()> {
        let ev = libc::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &ev as *const _ as *const u8,
                std::mem::size_of::<libc::input_event>(),
            )
        };
        self.file
            .as_mut()
            .ok_or("uinput disabled after an injection error; restart Wispr Flow")?
            .write_all(bytes)
            .map_err(|e| format!("uinput write: {e}"))
    }

    fn syn(&mut self) -> Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }

    /// Press (value=1) or release (value=0) a single evdev key, with a SYN.
    pub fn key(&mut self, code: u16, press: bool) -> Result<()> {
        self.emit(EV_KEY, code, if press { 1 } else { 0 })?;
        self.syn()
    }

    /// Press a chord: hold `mods` (in order), tap `key`, release everything in
    /// reverse.
    ///
    /// CRITICAL: the modifier-down → key-down → key-up → modifier-up events are
    /// emitted as one *contiguous* batch with **no inter-event sleep**. On
    /// KWin/Wayland a quiescent gap after a virtual modifier-down causes the
    /// compositor to drop the modifier before the key arrives, so an injected
    /// Ctrl+V degrades to a bare `v` (the entire paste path silently failed this
    /// way). Counter-intuitively, an "observe the modifier" delay here is the
    /// bug, not the fix — verified: 0 ms → modifier applied, ≥8 ms → dropped.
    /// See docs/learnings/wayland-injection.md.
    ///
    /// Portal mode never probes physical keyboard state, including at paste
    /// time. The historical modifier snapshot is only available in explicitly
    /// selected legacy evdev mode. Release physical shortcut keys before paste.
    /// On any write/SYN failure, try all outstanding releases and destroy the
    /// virtual device. Continuing on an uncertain key state risks stuck keys.
    pub fn chord(&mut self, key: u16, mods: &[u16]) -> Result<()> {
        let held = held_modifiers();
        let result = inject_chord(key, mods, &held, |code, press| self.key(code, press));
        if result.is_err() {
            self.destroy();
        }
        result
    }

    fn destroy(&mut self) {
        if let Some(file) = self.file.take() {
            unsafe { libc::ioctl(file.as_raw_fd(), UI_DEV_DESTROY) };
            // Dropping the fd is also the kernel's cleanup path if ioctl failed.
        }
    }
}

/// Keep emission separately testable so every write/SYN failure boundary can
/// be exercised without a privileged virtual keyboard. Track before pressing:
/// the key write may succeed even if its following SYN_REPORT fails.
fn inject_chord(
    key: u16,
    mods: &[u16],
    held: &[u16],
    mut emit: impl FnMut(u16, bool) -> Result<()>,
) -> Result<()> {
    let mut down = Vec::new();
    let result = (|| {
        for &modifier in held {
            emit(modifier, false)?;
        }
        for &modifier in mods {
            if !down.contains(&modifier) {
                down.push(modifier);
                emit(modifier, true)?;
            }
        }
        if !down.contains(&key) {
            down.push(key);
        }
        emit(key, true)?;
        while let Some(&code) = down.last() {
            emit(code, false)?;
            down.pop();
        }
        // Retain the legacy snapshot/restore behavior only after a complete
        // chord; on failure the caller destroys the whole virtual device.
        for &modifier in held.iter().rev() {
            down.push(modifier);
            emit(modifier, true)?;
        }
        Ok(())
    })();
    if result.is_err() {
        for &code in down.iter().rev() {
            let _ = emit(code, false);
        }
    }
    result
}

fn legacy_snapshot(mode: Option<&str>, scan: impl FnOnce() -> Vec<u16>) -> Vec<u16> {
    if mode == Some("evdev") {
        scan()
    } else {
        Vec::new()
    }
}

/// Raw physical state is available only in explicit legacy evdev mode.
/// Default, portal, none, xinput and invalid modes do not even list /dev/input.
pub fn held_modifiers() -> Vec<u16> {
    legacy_snapshot(
        std::env::var("WISPR_KEY_CAPTURE").ok().as_deref(),
        scan_modifiers,
    )
}

fn scan_modifiers() -> Vec<u16> {
    use std::collections::BTreeSet;
    // EVIOCGKEY(len) = _IOC(_IOC_READ=2, 'E'=0x45, 0x18, len). KEY_MAX=0x2ff ->
    // a 96-byte bitmap covers every keycode we care about.
    const BITMAP_LEN: usize = (KEY_MAX as usize / 8) + 1;
    let req: libc::c_ulong =
        ((2u64 << 30) | ((BITMAP_LEN as u64) << 16) | (0x45 << 8) | 0x18) as libc::c_ulong;

    let dir = match std::fs::read_dir("/dev/input") {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut held = BTreeSet::new();
    for entry in dir.flatten() {
        let path = entry.path();
        let is_event = path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.starts_with("event"));
        if !is_event {
            continue;
        }
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(f) => f,
            Err(_) => continue, // not readable -> skip this device
        };
        let mut bitmap = [0u8; BITMAP_LEN];
        let r = unsafe { libc::ioctl(file.as_raw_fd(), req, bitmap.as_mut_ptr()) };
        if r < 0 {
            continue;
        }
        for &m in keymap::EVDEV_MODIFIERS {
            let (byte, bit) = (m as usize / 8, m as u32 % 8);
            if byte < bitmap.len() && (bitmap[byte] >> bit) & 1 == 1 {
                held.insert(m);
            }
        }
    }
    held.into_iter().collect()
}

impl Drop for UInput {
    fn drop(&mut self) {
        self.destroy();
    }
}

fn ioctl_set(fd: libc::c_int, req: libc::c_ulong, arg: libc::c_int) -> Result<()> {
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        return Err(format!(
            "ioctl {req:#x}({arg}): {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn physical_snapshot_is_never_called_outside_explicit_evdev() {
        for mode in [
            None,
            Some("portal"),
            Some("none"),
            Some("xinput"),
            Some(""),
            Some("typo"),
        ] {
            assert!(legacy_snapshot(mode, || panic!("physical keyboard read")).is_empty());
        }
        assert_eq!(legacy_snapshot(Some("evdev"), || vec![29]), vec![29]);
    }

    #[test]
    fn chord_releases_every_key_at_every_failure_boundary() {
        // Each emit models a successful key write followed by a failing SYN.
        // It also covers a failing release and verifies the cleanup retries it.
        for fail_at in 0..6 {
            let mut held = BTreeSet::new();
            let mut call = 0;
            let result = inject_chord(47, &[29, 42], &[], |code, press| {
                if press {
                    held.insert(code);
                } else {
                    held.remove(&code);
                }
                let fail = call == fail_at;
                call += 1;
                if fail {
                    Err("injected write/SYN failure".into())
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err(), "failure point {fail_at}");
            assert!(held.is_empty(), "stuck key at failure point {fail_at}");
        }
    }

    #[test]
    fn successful_chord_has_no_duplicate_modifiers_and_reverses_release_order() {
        let mut events = Vec::new();
        inject_chord(47, &[29, 29, 42], &[], |code, press| {
            events.push((code, press));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            events,
            vec![
                (29, true),
                (42, true),
                (47, true),
                (47, false),
                (42, false),
                (29, false)
            ]
        );
    }

    #[test]
    fn failed_device_is_closed_and_cannot_accept_another_chord() {
        // /dev/full is an unprivileged kernel ENOSPC sink, not an input device.
        let mut input = UInput {
            file: Some(OpenOptions::new().write(true).open("/dev/full").unwrap()),
        };
        assert!(input.chord(47, &[29]).is_err());
        assert!(input.file.is_none());
        assert!(input.chord(47, &[29]).is_err());
    }
}
