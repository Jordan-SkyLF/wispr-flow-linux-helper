//! XDG GlobalShortcuts v1+ backend. Never opens a keyboard device.
//!
//! The compositor sends action activation/release, not global keystrokes. We
//! translate only approved action IDs into the existing helper IPC contract.
//! A separate thread keeps portal permission dialogs off the IsReady path.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::{atomic::AtomicU64, Arc, Mutex};

use serde::Deserialize;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::{Message, Type};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::portal_state::ShortcutState;
use super::{emit_keypress, HeldKeys};
use crate::backend::EventSink;

const SERVICE: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
// Default native-package desktop basename; AppImage overrides this identity.
const APP_ID: &str = "wispr-flow";
type Error = Box<dyn std::error::Error + Send + Sync>;
type Results = HashMap<String, OwnedValue>;
type BoundShortcuts = Vec<(String, Results)>;
type State = Arc<Mutex<ShortcutState>>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    id: String,
    description: String,
    preferred_trigger: String,
    keys: Vec<u32>,
}

fn bindings_from_json(json: Option<&str>) -> Result<Vec<Binding>, Error> {
    let bindings = match json {
        Some(text) if text.len() <= 65_536 => serde_json::from_str(text)?,
        Some(_) => return Err("WISPR_PORTAL_SHORTCUTS exceeds 64 KiB".into()),
        None => vec![
            Binding {
                id: "dictate".into(),
                description: "Wispr Flow: hold to dictate".into(),
                // Avoid held physical modifiers contaminating the later uinput
                // paste. The portal cannot report when those modifiers go up.
                preferred_trigger: "F8".into(),
                // The app's documented default: left Control + left Windows.
                keys: vec![162, 91],
            },
            Binding {
                id: "dismiss".into(),
                description: "Wispr Flow: cancel dictation".into(),
                preferred_trigger: "F9".into(),
                // Wispr checks Dismiss as a subset while PTT keys remain held.
                keys: vec![27],
            },
        ],
    };
    if bindings.is_empty() || bindings.len() > 16 {
        return Err("configure between 1 and 16 portal shortcuts".into());
    }
    let mut ids = HashSet::new();
    for b in &bindings {
        if b.id.is_empty()
            || b.id.len() > 64
            || !b
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            || !ids.insert(b.id.clone())
        {
            return Err("portal shortcut IDs must be unique ASCII identifiers".into());
        }
        if b.description.is_empty()
            || b.description.len() > 256
            || !valid_trigger(&b.preferred_trigger)
        {
            return Err(format!("invalid description or XDG trigger for {}", b.id).into());
        }
        if b.keys.is_empty()
            || b.keys.len() > 8
            || b.keys.iter().any(|vk| !(1..=255).contains(vk))
            || b.keys.iter().copied().collect::<HashSet<_>>().len() != b.keys.len()
        {
            return Err(format!("{} needs 1-8 distinct Windows VK codes (1-255)", b.id).into());
        }
    }
    Ok(bindings)
}

fn valid_trigger(trigger: &str) -> bool {
    if trigger.is_empty() || trigger.len() > 128 {
        return false;
    }
    let mut parts: Vec<&str> = trigger.split('+').collect();
    let key = parts.pop().unwrap_or_default();
    let mut seen = HashSet::new();
    !key.is_empty()
        && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        && parts.iter().all(|modifier| {
            matches!(*modifier, "CTRL" | "ALT" | "SHIFT" | "NUM" | "LOGO") && seen.insert(*modifier)
        })
}

struct PortalHeldKeys(State);
impl HeldKeys for PortalHeldKeys {
    fn held_vks(&self) -> HashSet<u32> {
        // We cannot query physical state without recreating the privacy issue.
        // Only portal-approved active chords count as held in this backend.
        self.0.lock().unwrap_or_else(|e| e.into_inner()).held()
    }
}

pub(super) fn start(events: EventSink) -> Result<Box<dyn HeldKeys>, Error> {
    let json = std::env::var("WISPR_PORTAL_SHORTCUTS").ok();
    let bindings = bindings_from_json(json.as_deref())?;
    let app_id = std::env::var("WISPR_PORTAL_APP_ID").unwrap_or_else(|_| APP_ID.into());
    if app_id.is_empty()
        || app_id.len() > 255
        || !app_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
    {
        return Err("WISPR_PORTAL_APP_ID must be a desktop-file basename without .desktop".into());
    }
    let state = Arc::new(Mutex::new(ShortcutState::default()));
    let worker_state = Arc::clone(&state);
    std::thread::Builder::new()
        .name("wispr-shortcuts-portal".into())
        .spawn(move || {
            let index = AtomicU64::new(0);
            let result = run(&events, &index, &worker_state, &bindings, &app_id);
            release_all(&events, &index, &worker_state);
            if let Err(error) = result {
                log::error!(
                    "GlobalShortcuts stopped: {error}. No raw-input fallback. \
                Install/configure the desktop portal backend and restart Wispr Flow."
                );
            }
        })?;
    Ok(Box::new(PortalHeldKeys(state)))
}

fn send(events: &EventSink, index: &AtomicU64, changes: Vec<(u32, bool)>) {
    for (vk, pressed) in changes {
        emit_keypress(events, index, std::process::id(), vk, pressed);
    }
}

fn release_all(events: &EventSink, index: &AtomicU64, state: &State) {
    let changes = state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .cancel_and_release_all();
    send(events, index, changes);
}

/// A request token must be unpredictable, not a PID or sequential counter.
fn token() -> Result<String, Error> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("wispr_{suffix}"))
}

/// Release IPC state before Close(), even if the D-Bus call fails or times out.
struct SessionGuard<'a> {
    connection: &'a Connection,
    owner: &'a str,
    path: OwnedObjectPath,
    events: &'a EventSink,
    index: &'a AtomicU64,
    state: &'a State,
}
impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        release_all(self.events, self.index, self.state);
        if let Ok(proxy) = Proxy::new(
            self.connection,
            self.owner,
            self.path.as_str(),
            "org.freedesktop.portal.Session",
        ) {
            let _: Result<(), _> = proxy.call("Close", &());
        }
    }
}

fn owner_lost(message: &Message, owner: &str) -> Result<bool, Error> {
    let h = message.header();
    if h.message_type() != Type::Signal
        || h.sender().map(|v| v.as_str()) != Some("org.freedesktop.DBus")
        || h.path().map(|v| v.as_str()) != Some("/org/freedesktop/DBus")
        || h.interface().map(|v| v.as_str()) != Some("org.freedesktop.DBus")
        || h.member().map(|v| v.as_str()) != Some("NameOwnerChanged")
    {
        return Ok(false);
    }
    let (name, old, new): (String, String, String) = message.body().deserialize()?;
    Ok(name == SERVICE && old == owner && new != owner)
}

/// The iterator is installed before calling a portal method, so an immediate
/// Request.Response cannot race past us. The actual returned request path is
/// used, rather than assuming the portal honored our preferred handle token.
fn response(
    iter: &mut MessageIterator,
    owner: &str,
    request: &OwnedObjectPath,
    session: Option<&OwnedObjectPath>,
) -> Result<Results, Error> {
    for message in iter.by_ref() {
        let message = message?;
        if owner_lost(&message, owner)? {
            return Err("portal service exited".into());
        }
        let h = message.header();
        if h.sender().map(|v| v.as_str()) == Some(owner)
            && h.interface().map(|v| v.as_str()) == Some("org.freedesktop.portal.Session")
            && h.member().map(|v| v.as_str()) == Some("Closed")
            && session.is_some_and(|s| h.path().map(|v| v.as_str()) == Some(s.as_str()))
        {
            return Err("shortcut session closed while awaiting permission".into());
        }
        if h.message_type() != Type::Signal
            || h.sender().map(|v| v.as_str()) != Some(owner)
            || h.path().map(|v| v.as_str()) != Some(request.as_str())
            || h.interface().map(|v| v.as_str()) != Some("org.freedesktop.portal.Request")
            || h.member().map(|v| v.as_str()) != Some("Response")
        {
            continue;
        }
        let (code, results): (u32, Results) = message.body().deserialize()?;
        return match code {
            0 => Ok(results),
            1 => Err("shortcut permission was cancelled".into()),
            _ => Err(format!("portal request failed (response {code})").into()),
        };
    }
    Err("session bus disconnected".into())
}

fn run(
    events: &EventSink,
    index: &AtomicU64,
    state: &State,
    bindings: &[Binding],
    app_id: &str,
) -> Result<(), Error> {
    let connection = Connection::session()?;
    let bus = Proxy::new(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )?;
    // Activate only if no owner exists: a manually started portal can own the
    // name without a .service activation file. Pin all subsequent calls,
    // including Register, so a restart cannot cross between two processes.
    let owner: String = match bus.call("GetNameOwner", &(SERVICE,)) {
        Ok(owner) => owner,
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner" =>
        {
            let _: u32 = bus.call("StartServiceByName", &(SERVICE, 0u32))?;
            bus.call("GetNameOwner", &(SERVICE,))?
        }
        Err(error) => return Err(error.into()),
    };
    // Validate the unique name before interpolating it in a D-Bus match rule.
    let _ = zbus::names::UniqueName::try_from(owner.as_str())?;
    let portal = Proxy::new(&connection, owner.as_str(), PATH, INTERFACE)?;

    // A single unfiltered local iterator receives only messages delivered to
    // this connection. The bus subscriptions below are narrow: this portal's
    // signals and its owner-change notification, not all session-bus signals.
    let mut iter = MessageIterator::from(&connection);
    let _: () = bus.call(
        "AddMatch",
        &(format!(
            "type='signal',sender='{owner}',path_namespace='{PATH}'"
        ),),
    )?;
    let _: () = bus.call(
        "AddMatch",
        &(format!(
            "type='signal',sender='org.freedesktop.DBus',\
         interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0='{SERVICE}'"
        ),),
    )?;
    // Cover a service restart between GetNameOwner and the subscription.
    let current_owner: String = bus.call("GetNameOwner", &(SERVICE,))?;
    if current_owner != owner {
        return Err("portal restarted during setup".into());
    }

    // Host apps must register on this same connection before any portal API.
    // Otherwise KDE may persist bindings under a new random session token on
    // every launch. XDP validates that the selected app ID has a desktop file.
    let registry = Proxy::new(
        &connection,
        owner.as_str(),
        PATH,
        "org.freedesktop.host.portal.Registry",
    )?;
    let options: HashMap<&str, Value<'_>> = HashMap::new();
    match registry.call::<_, _, ()>("Register", &(app_id, options)) {
        Ok(()) => {}
        Err(zbus::Error::MethodError(name, _, _))
            if matches!(
                name.as_str(),
                "org.freedesktop.DBus.Error.UnknownMethod"
                    | "org.freedesktop.DBus.Error.UnknownInterface"
            ) =>
        {
            log::warn!(
                "Host Registry unavailable; using desktop-derived portal identity. \
                Upgrade xdg-desktop-portal for reliable shortcut persistence."
            );
        }
        Err(error) => {
            return Err(format!(
                "portal app registration failed: {error}; \
            ensure {app_id}.desktop is installed"
            )
            .into())
        }
    }
    let activation = Proxy::new(&connection, owner.as_str(), PATH, INTERFACE)?;
    let version: u32 = activation.get_property("version")?;
    if version < 1 {
        return Err("GlobalShortcuts v1 or newer is required".into());
    }

    let request_token = token()?;
    let session_token = token()?;
    let options = HashMap::from([
        ("handle_token", Value::from(request_token.as_str())),
        ("session_handle_token", Value::from(session_token.as_str())),
    ]);
    let request: OwnedObjectPath = portal.call("CreateSession", &(options,))?;
    let mut created = response(&mut iter, &owner, &request, None)?;
    // The protocol deliberately encodes session_handle as 's', NOT 'o'.
    let session_path = String::try_from(
        created
            .remove("session_handle")
            .ok_or("CreateSession omitted session_handle")?,
    )?;
    let session = SessionGuard {
        connection: &connection,
        owner: &owner,
        path: OwnedObjectPath::try_from(session_path)?,
        events,
        index,
        state,
    };

    let shortcuts: Vec<_> = bindings
        .iter()
        .map(|b| {
            (
                b.id.as_str(),
                HashMap::from([
                    ("description", Value::from(b.description.as_str())),
                    (
                        "preferred_trigger",
                        Value::from(b.preferred_trigger.as_str()),
                    ),
                ]),
            )
        })
        .collect();
    let request_token = token()?;
    let options = HashMap::from([("handle_token", Value::from(request_token.as_str()))]);
    // Bind exactly once per session. Do not silently retry a denial.
    let request: OwnedObjectPath =
        portal.call("BindShortcuts", &(&session.path, shortcuts, "", options))?;
    let mut results = response(&mut iter, &owner, &request, Some(&session.path))?;
    let approved = BoundShortcuts::try_from(
        results
            .remove("shortcuts")
            .ok_or("BindShortcuts omitted shortcuts")?,
    )?;
    let admitted = admitted_bindings(approved, bindings);
    if admitted.is_empty() {
        return Err("no requested shortcut was approved".into());
    }
    *state.lock().unwrap_or_else(|e| e.into_inner()) =
        ShortcutState::new(admitted, cancel_keys(bindings));
    log::info!("key capture: GlobalShortcuts v{version}; no keyboard-device access");

    for message in iter {
        let message = message?;
        if owner_lost(&message, &owner)? {
            return Err("portal service exited".into());
        }
        let h = message.header();
        if h.message_type() != Type::Signal
            || h.sender().map(|v| v.as_str()) != Some(owner.as_str())
        {
            continue;
        }
        if h.interface().map(|v| v.as_str()) == Some("org.freedesktop.portal.Session")
            && h.path().map(|v| v.as_str()) == Some(session.path.as_str())
            && h.member().map(|v| v.as_str()) == Some("Closed")
        {
            return Err("shortcut session closed".into());
        }
        if h.interface().map(|v| v.as_str()) != Some(INTERFACE)
            || h.path().map(|v| v.as_str()) != Some(PATH)
        {
            continue;
        }
        if h.member().map(|v| v.as_str()) == Some("ShortcutsChanged") {
            let (changed_session, shortcuts): (OwnedObjectPath, BoundShortcuts) =
                message.body().deserialize()?;
            if changed_session == session.path {
                // A rebind can remove a held shortcut without a Deactivated.
                // Clear first, then admit only still-bound configured actions.
                release_all(events, index, state);
                *state.lock().unwrap_or_else(|e| e.into_inner()) = ShortcutState::new(
                    admitted_bindings(shortcuts, bindings),
                    cancel_keys(bindings),
                );
            }
            continue;
        }
        let pressed = match h.member().map(|v| v.as_str()) {
            Some("Activated") => true,
            Some("Deactivated") => false,
            _ => continue,
        };
        let (signal_session, id, timestamp, _options): (OwnedObjectPath, String, u64, Results) =
            message.body().deserialize()?;
        if signal_session != session.path {
            continue;
        }
        let changes = state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .transition(&id, pressed, timestamp);
        send(events, index, changes);
    }
    Err("session bus disconnected".into())
}

fn cancel_keys(bindings: &[Binding]) -> Vec<u32> {
    bindings
        .iter()
        .find(|binding| binding.id == "dismiss")
        .map(|binding| binding.keys.clone())
        .unwrap_or_else(|| vec![27])
}

fn admitted_bindings(approved: BoundShortcuts, bindings: &[Binding]) -> HashMap<String, Vec<u32>> {
    let allowed: HashSet<String> = approved
        .into_iter()
        .filter_map(|(id, properties)| {
            // An empty trigger is an unbound shortcut, not usable consent.
            let trigger = properties
                .get("trigger_description")
                .and_then(|value| <&str>::try_from(value).ok());
            trigger.filter(|value| !value.trim().is_empty()).map(|_| id)
        })
        .collect();
    bindings
        .iter()
        .filter(|binding| allowed.contains(&binding.id))
        .map(|binding| (binding.id.clone(), binding.keys.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_documented_app_chord() {
        let bindings = bindings_from_json(None).unwrap();
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].keys, vec![162, 91]);
        assert_eq!(bindings[0].preferred_trigger, "F8");
        assert_eq!(bindings[1].preferred_trigger, "F9");
        assert_eq!(bindings[1].keys, vec![27]);
    }

    #[test]
    fn malformed_configuration_is_rejected() {
        for text in ["", "[]", "{}", "null", "not json"] {
            assert!(bindings_from_json(Some(text)).is_err());
        }
    }

    #[test]
    fn invalid_or_duplicate_keys_are_rejected() {
        for keys in ["[]", "[0]", "[256]", "[162,162]"] {
            let json = format!(
                r#"[{{"id":"a","description":"Dictate","preferred_trigger":"CTRL+d","keys":{keys}}}]"#
            );
            assert!(bindings_from_json(Some(&json)).is_err());
        }
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let b =
            r#"{"id":"a","description":"Dictate","preferred_trigger":"CTRL+d","keys":[162,91]}"#;
        assert!(bindings_from_json(Some(&format!("[{b},{b}]"))).is_err());
    }

    #[test]
    fn trigger_uses_xdg_names_not_gtk_accelerator_syntax() {
        for trigger in ["CTRL+LOGO+d", "CTRL+ALT+Return", "F8", "a"] {
            assert!(valid_trigger(trigger), "{trigger}");
        }
        for trigger in [
            "",
            "CTRL+",
            "SUPER+d",
            "CTRL+CTRL+d",
            "<Ctrl>d",
            "CTRL+bad key",
        ] {
            assert!(!valid_trigger(trigger), "{trigger}");
        }
    }
}
