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
//! `/dev/uinput` (granted to the active-session user via a logind `uaccess`
//! udev rule / ACL). This permits synthetic input, not physical keyboard reads.
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
    // Closing the fd retires the device even if UI_DEV_DESTROY fails.
    file: Option<File>,
    // Includes presses whose write or following SYN might have failed, and
    // legacy modifiers restored by an earlier successful chord.
    down: Vec<u16>,
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
            .map_err(|e| format!("open /dev/uinput: {e} (need the installed logind uaccess rule and an active local session)"))?;
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

        Ok(UInput {
            file: Some(file),
            down: Vec::new(),
        })
    }

    /// Any failed event or SYN makes this device unsafe to reuse.
    fn key(&mut self, code: u16, press: bool) -> Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or("uinput disabled after an injection error; restart Wispr Flow")?;
        let result = write_key(
            file,
            &mut self.down,
            code,
            press,
            crate::capture::injection_allowed,
        );
        if result.is_err() {
            self.destroy();
        }
        result
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
    /// Physical modifiers are inspected only in explicitly selected evdev
    /// capture mode. Portal operation does not read `/dev/input`, including at
    /// paste time; release physical shortcut keys before insertion. The legacy
    /// snapshot/release/restore behavior is retained for evdev users.
    ///
    /// On a failed write or SYN, attempt every outstanding synthetic release,
    /// then close/destroy the virtual keyboard and reject further injection.
    pub fn chord(&mut self, key: u16, mods: &[u16]) -> Result<()> {
        if self.file.is_none() {
            return Err("uinput disabled after an injection error; restart Wispr Flow".into());
        }
        if !crate::capture::injection_allowed() {
            // Initial portal consent can still succeed. An untouched device
            // remains usable then; existing synthetic holds need cleanup now.
            if !self.down.is_empty() {
                self.destroy();
            }
            return Err("uinput blocked because portal capture is unavailable".into());
        }
        let held = held_modifiers();
        inject_chord(key, mods, &held, |code, press| self.key(code, press))
    }

    fn destroy(&mut self) {
        if let Some(mut file) = self.file.take() {
            release_keys(&mut file, &mut self.down);
            unsafe { libc::ioctl(file.as_raw_fd(), UI_DEV_DESTROY) };
            // File drops here even when the ioctl fails.
        }
    }
}

fn emit(writer: &mut impl Write, type_: u16, code: u16, value: i32) -> Result<()> {
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
    writer
        .write_all(bytes)
        .map_err(|e| format!("uinput write: {e}"))
}

/// Keep the key/SYN write boundary testable without a privileged input device.
/// Track presses before writing; retain releases until their SYN succeeds.
fn write_key(
    writer: &mut impl Write,
    down: &mut Vec<u16>,
    code: u16,
    press: bool,
    injection_allowed: impl FnOnce() -> bool,
) -> Result<()> {
    if press {
        if !injection_allowed() {
            release_keys(writer, down);
            return Err("uinput blocked because portal capture is unavailable".into());
        }
        if down.contains(&code) {
            return Ok(());
        }
        down.push(code);
    }
    let result = emit(writer, EV_KEY, code, i32::from(press))
        .and_then(|()| emit(writer, EV_SYN, SYN_REPORT, 0));
    if result.is_err() {
        release_keys(writer, down);
    } else if !press {
        down.retain(|&held| held != code);
    }
    result
}

fn release_keys(writer: &mut impl Write, down: &mut Vec<u16>) {
    for code in down.drain(..).rev() {
        // A failed release must not prevent releasing the remaining keys, or
        // attempting SYN for events the kernel may already have accepted.
        let _ = emit(writer, EV_KEY, code, 0);
        let _ = emit(writer, EV_SYN, SYN_REPORT, 0);
    }
}

fn inject_chord(
    key: u16,
    mods: &[u16],
    held: &[u16],
    mut emit_key: impl FnMut(u16, bool) -> Result<()>,
) -> Result<()> {
    for &modifier in held {
        emit_key(modifier, false)?;
    }
    let mut keys = Vec::with_capacity(mods.len() + 1);
    for &modifier in mods {
        if modifier != key && !keys.contains(&modifier) {
            keys.push(modifier);
        }
    }
    keys.push(key);
    for &code in &keys {
        emit_key(code, true)?;
    }
    for &code in keys.iter().rev() {
        emit_key(code, false)?;
    }
    for &modifier in held.iter().rev() {
        emit_key(modifier, true)?;
    }
    Ok(())
}

fn legacy_snapshot(allowed: bool, scan: impl FnOnce() -> Vec<u16>) -> Vec<u16> {
    if allowed {
        scan()
    } else {
        Vec::new()
    }
}

/// Only explicit evdev capture permits even listing physical input devices.
pub fn held_modifiers() -> Vec<u16> {
    legacy_snapshot(crate::capture::physical_input_allowed(), scan_modifiers)
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

    type Event = (u16, u16, i32);

    fn decode(bytes: &[u8]) -> Event {
        assert_eq!(bytes.len(), std::mem::size_of::<libc::input_event>());
        // A byte slice is not guaranteed to have input_event's alignment.
        let event = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<libc::input_event>()) };
        (event.type_, event.code, event.value)
    }

    #[derive(Default)]
    struct DeviceWriter {
        attempts: Vec<Event>,
        down: BTreeSet<u16>,
        fail_at: Option<usize>,
        permanent: bool,
    }

    impl Write for DeviceWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let event = decode(bytes);
            let index = self.attempts.len();
            self.attempts.push(event);
            if self
                .fail_at
                .is_some_and(|at| index == at || (self.permanent && index > at))
            {
                return Err(std::io::Error::other("injected device write failure"));
            }
            if event.0 == EV_KEY {
                if event.2 == 1 {
                    self.down.insert(event.1);
                } else {
                    self.down.remove(&event.1);
                }
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn chord(writer: &mut DeviceWriter, down: &mut Vec<u16>, held: &[u16]) -> Result<()> {
        inject_chord(47, &[29, 42], held, |code, press| {
            write_key(writer, down, code, press, || true)
        })
    }

    #[test]
    fn physical_snapshot_never_runs_without_explicit_evdev_permission() {
        assert!(legacy_snapshot(false, || panic!("physical keyboard read")).is_empty());
        assert_eq!(legacy_snapshot(true, || vec![29]), vec![29]);
    }

    #[test]
    fn normal_chord_order_is_preserved_with_no_duplicate_modifiers() {
        let mut writer = DeviceWriter::default();
        let mut down = Vec::new();
        inject_chord(47, &[29, 42, 29], &[], |code, press| {
            write_key(&mut writer, &mut down, code, press, || true)
        })
        .unwrap();
        assert_eq!(
            writer.attempts,
            vec![
                (EV_KEY, 29, 1),
                (EV_SYN, 0, 0),
                (EV_KEY, 42, 1),
                (EV_SYN, 0, 0),
                (EV_KEY, 47, 1),
                (EV_SYN, 0, 0),
                (EV_KEY, 47, 0),
                (EV_SYN, 0, 0),
                (EV_KEY, 42, 0),
                (EV_SYN, 0, 0),
                (EV_KEY, 29, 0),
                (EV_SYN, 0, 0),
            ]
        );
        assert!(writer.down.is_empty());
        assert!(down.is_empty());
    }

    #[test]
    fn main_key_is_not_pressed_twice_when_also_a_modifier() {
        let mut events = Vec::new();
        inject_chord(29, &[29, 29], &[], |code, press| {
            events.push((code, press));
            Ok(())
        })
        .unwrap();
        assert_eq!(events, vec![(29, true), (29, false)]);
    }

    #[test]
    fn every_key_and_syn_failure_releases_outstanding_keys() {
        // A plain chord has six key operations, each with a separate SYN.
        // Legacy snapshot/restore adds two operations for each held modifier.
        for held in [&[][..], &[56, 97][..]] {
            for fail_at in 0..(12 + held.len() * 4) {
                let mut writer = DeviceWriter {
                    fail_at: Some(fail_at),
                    ..DeviceWriter::default()
                };
                let mut down = Vec::new();
                assert!(chord(&mut writer, &mut down, held).is_err());
                assert!(writer.down.is_empty(), "failure {fail_at}, held {held:?}");
                assert!(down.is_empty());
            }
        }
    }

    #[test]
    fn legacy_restore_is_tracked_across_chords_and_released_after_failure() {
        let mut writer = DeviceWriter::default();
        let mut down = Vec::new();
        chord(&mut writer, &mut down, &[56, 97]).unwrap();
        assert_eq!(down, vec![97, 56]);
        assert_eq!(writer.down, BTreeSet::from([56, 97]));
        // Fail SYN after a new modifier press while previous restores exist.
        writer.fail_at = Some(writer.attempts.len() + 1);
        assert!(chord(&mut writer, &mut down, &[]).is_err());
        assert!(writer.down.is_empty());
        assert!(down.is_empty());
    }

    #[test]
    fn permanent_failure_attempts_every_release_and_syn() {
        let mut writer = DeviceWriter {
            fail_at: Some(0),
            permanent: true,
            ..DeviceWriter::default()
        };
        let mut down = vec![29, 42];
        assert!(write_key(&mut writer, &mut down, 47, true, || true).is_err());
        assert_eq!(
            writer.attempts,
            vec![
                (EV_KEY, 47, 1),
                (EV_KEY, 47, 0),
                (EV_SYN, 0, 0),
                (EV_KEY, 42, 0),
                (EV_SYN, 0, 0),
                (EV_KEY, 29, 0),
                (EV_SYN, 0, 0),
            ]
        );
        assert!(down.is_empty());
        // Releases are best-effort here; closing the device is still required.
    }

    #[test]
    fn partial_write_followed_by_error_attempts_uncertain_key_release() {
        #[derive(Default)]
        struct ShortWriter(Vec<Vec<u8>>);
        impl Write for ShortWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.push(bytes.to_vec());
                match self.0.len() {
                    1 => Ok(bytes.len() / 2),
                    2 => Err(std::io::Error::other("failure after short write")),
                    _ => Ok(bytes.len()),
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = ShortWriter::default();
        let mut down = Vec::new();
        assert!(write_key(&mut writer, &mut down, 47, true, || true).is_err());
        assert_eq!(writer.0.len(), 4);
        assert_eq!(writer.0[1].len(), writer.0[0].len() / 2);
        assert_eq!(decode(&writer.0[2]), (EV_KEY, 47, 0));
        assert_eq!(decode(&writer.0[3]), (EV_SYN, 0, 0));
        assert!(down.is_empty());
    }

    #[test]
    fn portal_failure_mid_chord_prevents_paste_and_releases_modifiers() {
        let mut writer = DeviceWriter::default();
        let mut down = Vec::new();
        let result = inject_chord(47, &[29], &[], |code, press| {
            // Portal failed after Control-down and before the paste key.
            write_key(&mut writer, &mut down, code, press, || code != 47)
        });
        assert!(result.is_err());
        assert!(!writer.attempts.contains(&(EV_KEY, 47, 1)));
        assert!(writer.down.is_empty());
        assert!(down.is_empty());
    }

    #[test]
    fn portal_failure_does_not_block_releases() {
        let mut writer = DeviceWriter::default();
        let mut down = Vec::new();
        write_key(&mut writer, &mut down, 29, true, || true).unwrap();
        write_key(&mut writer, &mut down, 29, false, || {
            panic!("release must not consult the capture guard")
        })
        .unwrap();
        assert!(writer.down.is_empty());
        assert!(down.is_empty());
    }

    #[test]
    fn real_write_failure_retires_the_device_and_rejects_future_injection() {
        // /dev/full is an unprivileged ENOSPC sink, not an input device.
        let mut input = UInput {
            file: Some(OpenOptions::new().write(true).open("/dev/full").unwrap()),
            down: vec![42],
        };
        // A release always reaches the real write, regardless of portal state.
        assert!(input.key(42, false).unwrap_err().contains("uinput write"));
        assert!(input.file.is_none());
        assert!(input.down.is_empty());
        assert!(input.key(47, true).unwrap_err().contains("disabled"));
        assert!(input.chord(47, &[29]).unwrap_err().contains("disabled"));
    }
}
