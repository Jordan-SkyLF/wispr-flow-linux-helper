//! Capture is separate from injection: portal on Wayland, XInput2 on true X11.
//! Physical evdev monitoring requires an explicit opt-in, never a fallback.

mod config;
mod evdev;
mod portal;
mod portal_state;
mod xinput;

use crate::backend::EventSink;
use serde_json::json;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

// Block insertion before fault cancellation reaches the app, including a
// transcription request already in flight. Portal approval opens this gate,
// including an idle session replacement; a terminal fault never retries.
// 0 = awaiting approval; 1 = permitted; 2 = terminal failure.
static INJECTION_STATE: AtomicU8 = AtomicU8::new(1);
pub(crate) fn injection_allowed() -> bool {
    INJECTION_STATE.load(Ordering::Acquire) == 1
}
pub(super) fn block_injection() {
    INJECTION_STATE.store(2, Ordering::Release);
}
pub(super) fn suspend_portal_injection() {
    let _ = INJECTION_STATE.compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire);
}
fn await_portal_approval() {
    let _ = INJECTION_STATE.compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire);
}
pub(super) fn allow_injection() {
    // An approval racing shutdown/failure must never reopen a terminal gate.
    let _ = INJECTION_STATE.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
}

pub trait HeldKeys {
    fn held_vks(&self) -> HashSet<u32>;
    // DictationStop is not completion: processing and paste may follow.
    fn dictation_started(&self) {}
    fn paste_completed(&self) {}
    fn shortcuts_changed(&self) {}
    fn shutdown(&self) {}
}
struct NoHeldKeys;
impl HeldKeys for NoHeldKeys {
    fn held_vks(&self) -> HashSet<u32> {
        HashSet::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureMode {
    Portal,
    X11,
    Evdev,
    None,
}

fn resolve_mode(
    canonical: Option<&str>,
    legacy: Option<&str>,
    display: Option<&str>,
    wayland: Option<&str>,
    session: Option<&str>,
) -> Result<CaptureMode, String> {
    let value = canonical.or(legacy).unwrap_or("auto");
    let is_wayland = session == Some("wayland") || wayland.is_some_and(|s| !s.is_empty());
    let is_x11 = !is_wayland && display.is_some_and(|s| !s.is_empty());
    match value {
        "auto" => Ok(if is_wayland {
            CaptureMode::Portal
        } else if is_x11 {
            CaptureMode::X11
        } else {
            CaptureMode::None
        }),
        "portal" => Ok(CaptureMode::Portal),
        "evdev" => Ok(CaptureMode::Evdev),
        "none" => Ok(CaptureMode::None),
        "x11" if is_x11 => Ok(CaptureMode::X11),
        "xinput" if canonical.is_none() && is_x11 => Ok(CaptureMode::X11),
        "x11" | "xinput" => Err("XInput2 requires a true X11 session and WISPR_CAPTURE=x11".into()),
        other => Err(format!(
            "unknown capture mode {other:?}; use WISPR_CAPTURE=auto|portal|x11|evdev|none"
        )),
    }
}
fn configured_mode() -> Result<CaptureMode, String> {
    resolve_mode(
        std::env::var("WISPR_CAPTURE").ok().as_deref(),
        std::env::var("WISPR_KEY_CAPTURE").ok().as_deref(),
        std::env::var("DISPLAY").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
    )
}
/// Paste-time modifier snapshots must honor the same explicit opt-in.
pub(crate) fn physical_input_allowed() -> bool {
    configured_mode() == Ok(CaptureMode::Evdev)
}
pub fn spawn(events: EventSink) -> Box<dyn HeldKeys> {
    if std::env::var_os("WISPR_KEY_CAPTURE").is_some() {
        log::warn!("WISPR_KEY_CAPTURE is deprecated; migrate to WISPR_CAPTURE. Canonical wins if both are set; xinput becomes x11.");
    }
    if std::env::var_os("WISPR_PORTAL_SHORTCUTS").is_some() {
        log::warn!("WISPR_PORTAL_SHORTCUTS is obsolete and ignored. Wispr settings own logical chords; KDE owns physical shortcuts.");
    }
    match configured_mode() {
        Ok(CaptureMode::Portal) => {
            await_portal_approval();
            match portal::start(events.clone()) {
                Ok(held) => held,
                Err(e) => {
                    portal::report_start_error(&events, &e);
                    log::error!("portal capture disabled: {e}; fix configuration and restart Wispr Flow. No raw-input fallback.");
                    Box::new(NoHeldKeys)
                }
            }
        }
        Ok(CaptureMode::X11) => match xinput::start(events) {
            Ok(held) => {
                log::info!("key capture: XInput2 (true X11)");
                held
            }
            Err(e) => {
                log::error!("XInput2 capture disabled: {e}; no evdev fallback");
                Box::new(NoHeldKeys)
            }
        },
        Ok(CaptureMode::Evdev) => {
            log::warn!("key capture: explicit legacy evdev; physical keyboard reads are enabled");
            evdev::start(events).unwrap_or_else(|| Box::new(NoHeldKeys))
        }
        Ok(CaptureMode::None) => {
            log::info!("key capture: disabled (no physical keyboard reads)");
            Box::new(NoHeldKeys)
        }
        Err(e) => {
            block_injection();
            log::error!("capture and insertion disabled: {e}");
            Box::new(NoHeldKeys)
        }
    }
}

fn emit_keypress(events: &EventSink, index: &AtomicU64, pid: u32, vk: u32, press: bool) {
    let idx = index.fetch_add(1, Ordering::Relaxed) + 1;
    let env = crate::proto::request(
        "KeypressEvent",
        json!({ "payload": {
            "eventType": if press { "key_event_press" } else { "key_event_release" },
            "key": vk, "index": idx, "inputType": "keyboard",
        }}),
        &format!("kp-{pid}-{idx}"),
    );
    let _ = events.send(env);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_never_infers_evdev_or_uses_xwayland_as_x11() {
        assert_eq!(
            resolve_mode(None, None, None, None, None),
            Ok(CaptureMode::None)
        );
        assert_eq!(
            resolve_mode(None, None, Some(":0"), None, None),
            Ok(CaptureMode::X11)
        );
        assert_eq!(
            resolve_mode(None, None, Some(":0"), Some("wayland-0"), None),
            Ok(CaptureMode::Portal)
        );
        assert_eq!(
            resolve_mode(None, None, Some(":0"), Some(""), Some("wayland")),
            Ok(CaptureMode::Portal)
        );
        assert_eq!(
            resolve_mode(None, None, Some(""), Some(""), None),
            Ok(CaptureMode::None)
        );
        assert!(resolve_mode(Some("x11"), None, Some(":0"), Some("wayland-0"), None).is_err());
        assert!(resolve_mode(Some("typo"), Some("evdev"), None, None, None).is_err());
        assert!(resolve_mode(Some(""), Some("evdev"), None, None, None).is_err());
    }
    #[test]
    fn modes_and_legacy_migration_have_unambiguous_precedence() {
        assert_eq!(
            resolve_mode(Some("evdev"), None, None, None, None),
            Ok(CaptureMode::Evdev)
        );
        assert_eq!(
            resolve_mode(None, Some("evdev"), None, None, None),
            Ok(CaptureMode::Evdev)
        );
        assert_eq!(
            resolve_mode(Some("portal"), Some("evdev"), None, None, None),
            Ok(CaptureMode::Portal)
        );
        assert_eq!(
            resolve_mode(None, Some("xinput"), Some(":0"), None, None),
            Ok(CaptureMode::X11)
        );
        assert!(resolve_mode(Some("xinput"), None, Some(":0"), None, None).is_err());
        assert_eq!(
            resolve_mode(Some("none"), Some("evdev"), Some(":0"), None, None),
            Ok(CaptureMode::None)
        );
    }
    #[test]
    fn keypress_frames_keep_the_wispr_contract() {
        let (tx, rx) = std::sync::mpsc::channel();
        let index = AtomicU64::new(0);
        emit_keypress(&tx, &index, 4242, 65, true);
        emit_keypress(&tx, &index, 4242, 65, false);
        let press = rx.recv().unwrap();
        let key = &press["HelperAPIRequest"]["KeypressEvent"]["payload"];
        assert_eq!(key["eventType"], "key_event_press");
        assert_eq!(key["key"], 65);
        assert_eq!(key["index"], 1);
        assert_eq!(key["inputType"], "keyboard");
        assert_eq!(press["HelperAPIRequest"]["uuid"], "kp-4242-1");
        let release = rx.recv().unwrap();
        assert_eq!(
            release["HelperAPIRequest"]["KeypressEvent"]["payload"]["index"],
            2
        );
    }
}
