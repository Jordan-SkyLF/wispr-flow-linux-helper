//! Shortcut events for Wispr Flow's existing KeypressEvent IPC contract.
//!
//! GlobalShortcuts is the default and fails closed: no keyboard devices are
//! opened if permission is denied, the portal is absent, or its session ends.
//! Only approved shortcut chords are synthesized. Unregistered keys are not
//! visible to the in-app shortcut recorder; configure physical keys in KDE.
//!
//! Legacy XInput2/evdev remain explicit WISPR_KEY_CAPTURE=xinput|evdev opt-ins.
//! They are not automatic fallbacks and the packages do not provision evdev
//! permissions. WISPR_KEY_CAPTURE=none disables global shortcut capture.

mod evdev;
mod portal;
mod portal_state;
mod xinput;

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;

use crate::backend::EventSink;

/// Report held Windows VK codes for `CheckStaleKeys`. Portal mode reports only
/// active, approved synthetic shortcut chords, not physical keyboard state.
/// Explicit legacy modes query evdev `EVIOCGKEY` or X11 `QueryKeymap`.
/// A key absent from this set is stale and may be dropped by the application.
pub trait HeldKeys {
    fn held_vks(&self) -> HashSet<u32>;
}

/// A held-keys querier that always reports nothing — used when no capture
/// backend is available (every queried key then reads as stale, which is the
/// safe answer: the app drops keys it can't confirm are held).
struct NoHeldKeys;
impl HeldKeys for NoHeldKeys {
    fn held_vks(&self) -> HashSet<u32> {
        HashSet::new()
    }
}

/// Start portal shortcut capture without blocking helper readiness. Raw input
/// is available only through an explicit legacy mode, never as a fallback.
/// The returned handle reports synthetic approved-chord state in portal mode.
pub fn spawn(events: EventSink) -> Box<dyn HeldKeys> {
    let mode = std::env::var("WISPR_KEY_CAPTURE").unwrap_or_else(|_| "portal".into());
    match mode.as_str() {
        "portal" => match portal::start(events) {
            Ok(held) => held,
            Err(error) => {
                log::error!("GlobalShortcuts unavailable: {error}; no raw-input fallback");
                Box::new(NoHeldKeys)
            }
        },
        "none" => Box::new(NoHeldKeys),
        "xinput" if is_true_x11_session() => {
            log::warn!("Explicit legacy XInput2 mode observes global keystrokes");
            match xinput::start(events) {
                Ok(held) => held,
                Err(error) => {
                    log::error!("XInput2 unavailable: {error}; no evdev fallback");
                    Box::new(NoHeldKeys)
                }
            }
        }
        "evdev" => {
            log::warn!("Explicit legacy evdev mode reads raw keyboard devices");
            evdev::start(events).unwrap_or_else(|| Box::new(NoHeldKeys))
        }
        _ => {
            log::error!("Unsupported key capture mode/session: {mode}; capture disabled");
            Box::new(NoHeldKeys)
        }
    }
}

/// True on an X11 session but not Wayland. On Wayland `DISPLAY` is usually also
/// set (XWayland), so require `WAYLAND_DISPLAY` to be absent.
fn is_true_x11_session() -> bool {
    std::env::var_os("DISPLAY").is_some() && std::env::var_os("WAYLAND_DISPLAY").is_none()
}

/// Emit one `KeypressEvent` on fd 3. `index` is a process-wide monotonic
/// sequence the app cross-checks against its own counter (it warns on a gap), so
/// every backend shares a single counter regardless of how many readers feed it.
fn emit_keypress(events: &EventSink, index: &AtomicU64, pid: u32, vk: u32, press: bool) {
    let idx = index.fetch_add(1, Ordering::Relaxed) + 1;
    let env = crate::proto::request(
        "KeypressEvent",
        json!({ "payload": {
            "eventType": if press { "key_event_press" } else { "key_event_release" },
            "key": vk,
            "index": idx,
            "inputType": "keyboard",
        } }),
        &format!("kp-{pid}-{idx}"),
    );
    let _ = events.send(env);
}

#[cfg(test)]
mod tests {
    use super::*;

    // The exact `KeypressEvent` frame shape is the contract the app's keyboard
    // service decodes; pin it so silent protocol drift fails the build.
    #[test]
    fn emit_keypress_builds_keypress_event_frame() {
        let (tx, rx) = std::sync::mpsc::channel();
        let index = AtomicU64::new(0);

        emit_keypress(&tx, &index, 4242, 65, true);
        emit_keypress(&tx, &index, 4242, 65, false);

        let press = rx.recv().expect("press frame");
        let kp = &press["HelperAPIRequest"]["KeypressEvent"]["payload"];
        assert_eq!(kp["eventType"], "key_event_press");
        assert_eq!(kp["key"], 65);
        assert_eq!(kp["index"], 1); // counter starts at 1, not 0
        assert_eq!(kp["inputType"], "keyboard");
        assert_eq!(press["HelperAPIRequest"]["uuid"], "kp-4242-1");

        let release = rx.recv().expect("release frame");
        let kp = &release["HelperAPIRequest"]["KeypressEvent"]["payload"];
        assert_eq!(kp["eventType"], "key_event_release");
        assert_eq!(kp["index"], 2); // shared monotonic counter advances
        assert_eq!(release["HelperAPIRequest"]["uuid"], "kp-4242-2");
    }

    // XInput2 is chosen only on a true X11 session: DISPLAY set and
    // WAYLAND_DISPLAY absent (under XWayland DISPLAY is also set, so its presence
    // alone must not select the X11 path).
    #[test]
    fn true_x11_requires_display_without_wayland() {
        // Snapshot + restore so the test leaves the process env untouched. No
        // other test reads these vars, so owning them here is race-free.
        let saved_display = std::env::var_os("DISPLAY");
        let saved_wayland = std::env::var_os("WAYLAND_DISPLAY");
        let restore = |key: &str, val: &Option<std::ffi::OsString>| match val {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        };

        // X11: DISPLAY set, no WAYLAND_DISPLAY.
        std::env::set_var("DISPLAY", ":0");
        std::env::remove_var("WAYLAND_DISPLAY");
        assert!(is_true_x11_session());

        // Wayland with XWayland: both set -> not a true X11 session.
        std::env::set_var("WAYLAND_DISPLAY", "wayland-0");
        assert!(!is_true_x11_session());

        // Headless: neither set.
        std::env::remove_var("DISPLAY");
        std::env::remove_var("WAYLAND_DISPLAY");
        assert!(!is_true_x11_session());

        restore("DISPLAY", &saved_display);
        restore("WAYLAND_DISPLAY", &saved_wayland);
    }
}
