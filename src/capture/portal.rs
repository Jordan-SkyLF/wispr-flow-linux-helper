//! Async GlobalShortcuts transport on a dedicated Tokio runtime (upstream D-008).
//! One ordered message stream preserves press/release ordering. The portal owns
//! physical bindings; Wispr's config owns the logical IPC chords we synthesize.

use super::config::{self, LogicalShortcuts};
use super::portal_state::{ShortcutState, CANCEL, PTT};
use super::{emit_keypress, HeldKeys};
use crate::backend::EventSink;
use futures_util::{pin_mut, StreamExt};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zbus::message::{Message, Type};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, MessageStream, Proxy};

const SERVICE: &str = "org.freedesktop.portal.Desktop";
const KDE_SERVICE: &str = "org.freedesktop.impl.portal.desktop.kde";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const GS_IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const SESSION_IFACE: &str = "org.freedesktop.portal.Session";
const DBUS: &str = "org.freedesktop.DBus";
const APP_ID: &str = "ai.wisprflow.Flow";
const CONFIG_POLL: Duration = Duration::from_secs(3);
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const CONSENT_TIMEOUT: Duration = Duration::from_secs(120);
type Results = HashMap<String, OwnedValue>;
type BoundShortcuts = Vec<(String, Results)>;

struct PortalStream {
    messages: MessageStream,
    kde_owner: Option<String>,
    // Binding changes can precede BindShortcuts' method reply or Response.
    // Keep their wire order and apply them before allowing the first action.
    changes: Vec<Message>,
}
impl PortalStream {
    fn lifecycle(
        &self,
        msg: &Message,
        owner: &str,
        session: Option<&OwnedObjectPath>,
    ) -> Result<(), String> {
        lifecycle(msg, owner, session)?;
        if let Some(kde_owner) = &self.kde_owner {
            if is_signal(msg, DBUS, "/org/freedesktop/DBus", DBUS)
                && msg.header().member().map(|m| m.as_str()) == Some("NameOwnerChanged")
            {
                let (name, old, new): (String, String, String) =
                    msg.body().deserialize().map_err(|e| e.to_string())?;
                if name == KDE_SERVICE && old == *kde_owner && new != *kde_owner {
                    return Err("KDE portal backend owner changed or disconnected".into());
                }
            }
        }
        Ok(())
    }

    fn remember_change(&mut self, msg: Message, owner: &str) -> Result<(), String> {
        if is_signal(&msg, owner, PORTAL_PATH, GS_IFACE)
            && msg.header().member().map(|m| m.as_str()) == Some("ShortcutsChanged")
        {
            if self.changes.len() >= 128 {
                return Err("portal binding-change queue overflow".into());
            }
            self.changes.push(msg);
        }
        Ok(())
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<ShortcutState>,
    index: AtomicU64,
    dirty: AtomicBool,
    shutdown: AtomicBool,
}
struct PortalHeld {
    shared: Arc<Shared>,
    events: EventSink,
    config_path: PathBuf,
}
impl Shared {
    fn emit(&self, events: &EventSink, changes: Vec<(u32, bool)>) {
        for (vk, down) in changes {
            emit_keypress(events, &self.index, std::process::id(), vk, down);
        }
    }
    fn fault(&self, events: &EventSink, replacement: Option<&[u32]>) {
        super::block_injection();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let changes = state.fault(replacement);
        // Keep state + enqueue order under one lock so a stale-key response
        // cannot overtake the cancellation sequence on the shared fd-3 writer.
        self.emit(events, changes);
    }
}
impl HeldKeys for PortalHeld {
    fn held_vks(&self) -> HashSet<u32> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .held()
    }
    fn dictation_started(&self) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .work_possible = true;
    }
    fn paste_completed(&self) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .paste_completed();
    }
    fn shortcuts_changed(&self) {
        self.shared.dirty.store(true, Ordering::Release);
    }
    fn shutdown(&self) {
        self.shared.shutdown.store(true, Ordering::Release);
        terminal_fault(&self.events, &self.shared, &self.config_path);
    }
}

pub fn start(events: EventSink) -> Result<Box<dyn HeldKeys>, String> {
    let cfg_path = config::config_path().ok_or("HOME/XDG_CONFIG_HOME is missing")?;
    let app_id = std::env::var("WISPR_PORTAL_APP_ID").unwrap_or_else(|_| APP_ID.into());
    if !valid_app_id(&app_id) {
        return Err("WISPR_PORTAL_APP_ID needs a reverse-DNS desktop basename, e.g. ai.wisprflow.Flow (without .desktop)".into());
    }
    let shared = Arc::new(Shared::default());
    let worker = shared.clone();
    let output = events.clone();
    let config_path = cfg_path.clone();
    std::thread::Builder::new().name("key-capture-portal".into()).spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread().enable_all().build()
            .map_err(|e| format!("portal runtime: {e}"))
            .and_then(|rt| rt.block_on(run(&output, &worker, &cfg_path, &app_id)));
        terminal_fault(&output, &worker, &cfg_path);
        if let Err(error) = result {
            log::error!("GlobalShortcuts stopped: {error}. Capture and insertion are disabled until Wispr Flow restarts. No raw-input fallback.");
        }
    }).map_err(|e| format!("start portal worker: {e}"))?;
    Ok(Box::new(PortalHeld {
        shared,
        events,
        config_path,
    }))
}

fn valid_app_id(id: &str) -> bool {
    id.len() <= 255
        && !id.ends_with(".desktop")
        && id.split('.').count() >= 3
        && id.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
}

async fn wait_for_config(path: &Path, shared: &Shared) -> Result<LogicalShortcuts, String> {
    let mut previous = String::new();
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            return Err("helper shutting down".into());
        }
        match config::read_shortcuts(path) {
            Ok(logical) => return Ok(logical),
            Err(error) => {
                if error != previous {
                    log::warn!("portal waiting for Wispr shortcut configuration: {error}. Finish first-run setup or reset incompatible shortcuts in Wispr; capture and insertion remain off.");
                    previous = error;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn run(events: &EventSink, shared: &Shared, path: &Path, app_id: &str) -> Result<(), String> {
    let logical = wait_for_config(path, shared).await?;
    shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .logical = Some(logical.clone());
    let conn = tokio::time::timeout(CALL_TIMEOUT, Connection::session())
        .await
        .map_err(|_| "session bus connection timed out")?
        .map_err(|e| format!("session bus: {e}"))?;
    let bus = Proxy::new(&conn, DBUS, "/org/freedesktop/DBus", DBUS)
        .await
        .map_err(|e| e.to_string())?;
    let owner: String = match timed(bus.call("GetNameOwner", &(SERVICE,))).await {
        Ok(owner) => owner,
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner" =>
        {
            let _: u32 = timed(bus.call("StartServiceByName", &(SERVICE, 0u32)))
                .await
                .map_err(|e| e.to_string())?;
            timed(bus.call("GetNameOwner", &(SERVICE,)))
                .await
                .map_err(|e| e.to_string())?
        }
        Err(error) => return Err(format!("portal service owner: {error}")),
    };
    zbus::names::UniqueName::try_from(owner.as_str()).map_err(|e| e.to_string())?;
    // Queue before subscriptions/calls. AddMatch limits delivered broadcasts;
    // local header checks remain mandatory, including for unicast signals.
    let mut messages = MessageStream::from(&conn);
    messages.set_max_queued(128);
    let mut stream = PortalStream {
        messages,
        kde_owner: None,
        changes: Vec::new(),
    };
    // The frontend can outlive KDE's backend without closing its sessions.
    // Pin that backend too, before creating a shortcut session, so its loss
    // cannot leave a held logical PTT recording until the five-minute limit.
    if std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case("KDE"))
    {
        let _: () = timed(bus.call("AddMatch", &(format!("type='signal',sender='{DBUS}',interface='{DBUS}',member='NameOwnerChanged',arg0='{KDE_SERVICE}'"),)))
            .await.map_err(|e| e.to_string())?;
        let kde_owner: String = timed(bus.call("GetNameOwner", &(KDE_SERVICE,)))
            .await
            .map_err(|e| format!("KDE portal backend owner: {e}"))?;
        zbus::names::UniqueName::try_from(kde_owner.as_str()).map_err(|e| e.to_string())?;
        stream.kde_owner = Some(kde_owner);
    }
    let _: () = timed(bus.call(
        "AddMatch",
        &(format!(
            "type='signal',sender='{owner}',path_namespace='{PORTAL_PATH}'"
        ),),
    ))
    .await
    .map_err(|e| e.to_string())?;
    let _: () = timed(bus.call("AddMatch",&(format!("type='signal',sender='{DBUS}',interface='{DBUS}',member='NameOwnerChanged',arg0='{SERVICE}'"),))).await.map_err(|e| e.to_string())?;
    let current: String = timed(bus.call("GetNameOwner", &(SERVICE,)))
        .await
        .map_err(|e| e.to_string())?;
    if current != owner {
        return Err("portal owner changed during setup".into());
    }
    let registry = Proxy::new(
        &conn,
        owner.as_str(),
        PORTAL_PATH,
        "org.freedesktop.host.portal.Registry",
    )
    .await
    .map_err(|e| e.to_string())?;
    let options: HashMap<&str, Value> = HashMap::new();
    match timed(registry.call::<_, _, ()>("Register", &(app_id, options))).await {
        Ok(()) => {}
        Err(zbus::Error::MethodError(name, _, _))
            if matches!(
                name.as_str(),
                "org.freedesktop.DBus.Error.UnknownMethod"
                    | "org.freedesktop.DBus.Error.UnknownInterface"
            ) =>
        {
            log::warn!("Host Registry unavailable; desktop-derived identity must resolve to {app_id}. Launch its installed desktop entry; consent/persistence remain unverified.");
        }
        Err(error) => {
            return Err(format!(
                "Host Registry: {error}; ensure {app_id}.desktop is installed and discoverable"
            ))
        }
    }
    let gs = Proxy::new(&conn, owner.as_str(), PORTAL_PATH, GS_IFACE)
        .await
        .map_err(|e| e.to_string())?;
    let version: u32 = timed(gs.get_property("version"))
        .await
        .map_err(|e| e.to_string())?;
    if version < 1 {
        return Err("GlobalShortcuts version 1 or later is required".into());
    }
    // Pin identity AND calls to this owner; drain its queued owner-loss event
    // inside portal_call so a restart during Register cannot silently succeed.
    let options = HashMap::from([
        ("handle_token", Value::from("wf_create")),
        ("session_handle_token", Value::from("wf_session")),
    ]);
    let mut created =
        portal_call(&mut stream, &gs, &owner, "CreateSession", &(options,), None).await?;
    // The spec intentionally uses a string variant, not an object-path variant.
    let session = String::try_from(
        created
            .remove("session_handle")
            .ok_or("CreateSession omitted session_handle")?,
    )
    .map_err(|e| format!("invalid session handle: {e}"))?;
    if !session.starts_with(&format!("{PORTAL_PATH}/session/")) {
        return Err("session handle outside portal namespace".into());
    }
    let session = OwnedObjectPath::try_from(session).map_err(|e| e.to_string())?;
    let result = run_session(
        events,
        shared,
        path,
        &logical,
        &mut stream,
        &gs,
        &owner,
        &session,
    )
    .await;
    terminal_fault(events, shared, path);
    if let Ok(proxy) = Proxy::new(&conn, owner.as_str(), session.as_str(), SESSION_IFACE).await {
        let _ = tokio::time::timeout(Duration::from_secs(2), proxy.call::<_, _, ()>("Close", &()))
            .await;
    }
    result
}

async fn timed<T>(future: impl std::future::Future<Output = zbus::Result<T>>) -> zbus::Result<T> {
    tokio::time::timeout(CALL_TIMEOUT, future)
        .await
        .map_err(|_| zbus::Error::Failure("D-Bus method timed out".into()))?
}

fn suggested_triggers(logical: &LogicalShortcuts) -> (String, String) {
    // Reuse an existing bare function-key choice at first registration. Other
    // chords get safe editable suggestions; physical modifiers cannot be
    // inspected without violating the portal privacy boundary.
    let function = |keys: &[u32]| {
        (keys.len() == 1 && (112..=135).contains(&keys[0]))
            .then(|| crate::keymap::chord_to_xdg_trigger(keys))
            .flatten()
    };
    let ptt = function(&logical.ptt).unwrap_or_else(|| "F8".into());
    let mut cancel = function(&logical.cancel).unwrap_or_else(|| "F9".into());
    if cancel == ptt {
        cancel = if ptt == "F9" {
            "F8".into()
        } else {
            "F9".into()
        };
    }
    (ptt, cancel)
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    events: &EventSink,
    shared: &Shared,
    config_path: &Path,
    initial: &LogicalShortcuts,
    stream: &mut PortalStream,
    gs: &Proxy<'_>,
    owner: &str,
    session: &OwnedObjectPath,
) -> Result<(), String> {
    let (ptt_trigger, cancel_trigger) = suggested_triggers(initial);
    let shortcuts = vec![
        (
            PTT,
            HashMap::from([
                ("description", Value::from("Wispr Flow: hold to dictate")),
                ("preferred_trigger", Value::from(ptt_trigger.as_str())),
            ]),
        ),
        (
            CANCEL,
            HashMap::from([
                ("description", Value::from("Wispr Flow: cancel dictation")),
                ("preferred_trigger", Value::from(cancel_trigger.as_str())),
            ]),
        ),
    ];
    let options = HashMap::from([("handle_token", Value::from("wf_bind"))]);
    // Bind once per new session, including when KDE has persisted shortcuts.
    let mut response = portal_call(
        stream,
        gs,
        owner,
        "BindShortcuts",
        &(session, shortcuts, "", options),
        Some(session),
    )
    .await?;
    let bound = BoundShortcuts::try_from(
        response
            .remove("shortcuts")
            .ok_or("BindShortcuts omitted shortcuts")?,
    )
    .map_err(|e| e.to_string())?;
    let mut registered = approved_bindings(bound)?;
    for msg in std::mem::take(&mut stream.changes) {
        let (changed, bound): (OwnedObjectPath, BoundShortcuts) =
            msg.body().deserialize().map_err(|e| e.to_string())?;
        if changed == *session {
            registered = approved_bindings(bound)?;
        }
    }
    // Consent may take minutes. Do not synthesize a cached chord after the app
    // changed settings while the dialog was open; pre-approval activations
    // have deliberately been discarded by portal_call.
    let latest = match config::read_shortcuts(config_path) {
        Ok(logical) => logical,
        Err(error) => {
            configuration_fault(events, shared, config_path);
            return Err(format!(
                "shortcut settings changed while awaiting permission: {error}"
            ));
        }
    };
    {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.faulted {
            return Err("capture already stopped during setup".into());
        }
        if state.logical.as_ref() != Some(&latest) && state.busy() {
            super::block_injection();
            let changes = state.fault(Some(&latest.cancel));
            shared.emit(events, changes);
            return Err(
                "shortcut settings changed during recording while awaiting permission".into(),
            );
        }
        state.logical = Some(latest);
        state.approved = registered.keys().cloned().collect();
        if registered.contains_key(PTT) {
            super::allow_injection();
        }
    }
    report_bindings(&registered);
    let mut observed = HashSet::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut next_config = Instant::now();
    loop {
        tokio::select! {
            message = stream.messages.next() => {
                let msg = message.ok_or("session bus disconnected")?.map_err(|e| format!("session bus disconnected: {e}"))?;
                stream.lifecycle(&msg,owner,Some(session))?;
                if !is_signal(&msg,owner,PORTAL_PATH,GS_IFACE) { continue; }
                match msg.header().member().map(|m| m.as_str()) {
                    Some("ShortcutsChanged") => {
                        let (changed,bound): (OwnedObjectPath,BoundShortcuts) = msg.body().deserialize().map_err(|e| e.to_string())?;
                        if changed != *session { continue; }
                        let new = approved_bindings(bound)?;
                        if new == registered { continue; }
                        if shared.state.lock().unwrap_or_else(|e| e.into_inner()).busy() {
                            return Err("KDE shortcuts changed during possible recording/processing; cancellation required".into());
                        }
                        if registered.contains_key(PTT) && !new.contains_key(PTT) {
                            return Err("KDE revoked or removed the active PTT binding".into());
                        }
                        shared.state.lock().unwrap_or_else(|e| e.into_inner()).approved = new.keys().cloned().collect();
                        registered = new;
                        observed.clear();
                        if registered.contains_key(PTT) { super::allow_injection(); }
                        report_bindings(&registered);
                    }
                    Some(member @ ("Activated"|"Deactivated")) => {
                        let (sig_session,id,_timestamp,_options): (OwnedObjectPath,String,u64,Results) = msg.body().deserialize().map_err(|e| e.to_string())?;
                        if sig_session != *session || !registered.contains_key(&id) { continue; }
                        let down = member=="Activated";
                        if down { synchronize_config(events, shared, config_path)?; }
                        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                        let changes = state.transition(&id,down,Instant::now());
                        shared.emit(events,changes);
                        if down && observed.insert(id.clone()) {
                            log::info!("portal action {id} activated by compositor; binding event delivery confirmed (recording/insertion still need desktop acceptance)");
                        }
                    }
                    _ => {}
                }
            }
            _ = tick.tick() => {
                if shared.shutdown.load(Ordering::Acquire) { return Ok(()); }
                if shared.state.lock().unwrap_or_else(|e| e.into_inner()).expired(Instant::now()) {
                    return Err("shortcut release missing or hold exceeded five minutes; cancelling rather than completing recording".into());
                }
                let dirty = shared.dirty.swap(false,Ordering::AcqRel);
                if dirty || Instant::now() >= next_config {
                    next_config = Instant::now()+CONFIG_POLL;
                    synchronize_config(events, shared, config_path)?;
                }
            }
        }
    }
}

fn synchronize_config(events: &EventSink, shared: &Shared, path: &Path) -> Result<(), String> {
    let new = match config::read_shortcuts(path) {
        Ok(logical) => logical,
        Err(error) => {
            configuration_fault(events, shared, path);
            return Err(format!(
                "Wispr shortcut configuration became unavailable or incompatible: {error}"
            ));
        }
    };
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.logical.as_ref() != Some(&new) {
        if state.busy() {
            super::block_injection();
            let changes = state.fault(Some(&new.cancel));
            shared.emit(events, changes);
            return Err("Wispr shortcuts changed during possible recording/processing; cancelled using the new logical Dismiss; restart required".into());
        }
        log::info!("Wispr logical shortcuts synchronized: PTT {:?}, Dismiss {:?}; KDE physical bindings retained", new.ptt, new.cancel);
        state.logical = Some(new);
    }
    Ok(())
}

fn terminal_fault(events: &EventSink, shared: &Shared, path: &Path) {
    super::block_injection();
    let current = {
        let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.faulted {
            return;
        }
        state.logical.clone()
    };
    match config::read_shortcuts(path) {
        Ok(latest) if current.as_ref() == Some(&latest) => shared.fault(events, None),
        Ok(latest) => shared.fault(events, Some(&latest.cancel)),
        Err(_) => configuration_fault(events, shared, path),
    }
}

fn configuration_fault(events: &EventSink, shared: &Shared, path: &Path) {
    match config::read_cancel(path) {
        Ok(cancel) => shared.fault(events, Some(&cancel)),
        Err(error) => {
            // Do not press the old cancellation mapping: the app may now bind
            // it to a different action. Insertion remains blocked. Empty new
            // cancel means release-only cleanup, never a guessed keypress.
            shared.fault(events, Some(&[]));
            log::error!("Current logical cancellation cannot be established ({error}); cancel any recording in the Wispr UI, then correct shortcuts and restart. Recording cancellation is NOT confirmed.");
        }
    }
}

fn report_bindings(bound: &HashMap<String, String>) {
    if let Some(trigger) = bound.get(PTT) {
        log::info!("GlobalShortcuts registered PTT {trigger:?}; awaiting a real compositor activation, not yet proof of a working shortcut");
    } else {
        log::warn!("PTT has no active trigger. Assign a shortcut to Wispr Flow in KDE System Settings > Keyboard > Shortcuts; capture/insertion remain off.");
    }
    if !bound.contains_key(CANCEL) {
        log::warn!("Cancel has no active KDE trigger. Assign a cancel shortcut in KDE; configured logical Dismiss is retained for fault cleanup.");
    }
}

fn approved_bindings(bound: BoundShortcuts) -> Result<HashMap<String, String>, String> {
    let mut result = HashMap::new();
    let mut seen = HashSet::new();
    for (id, properties) in bound {
        if !matches!(id.as_str(), PTT | CANCEL) {
            continue;
        }
        if !seen.insert(id.clone()) {
            return Err(format!("portal returned duplicate shortcut ID {id}"));
        }
        let trigger = properties
            .get("trigger_description")
            .and_then(|v| <&str>::try_from(v).ok());
        if let Some(trigger) = trigger
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("none"))
        {
            result.insert(id, trigger.into());
        }
    }
    Ok(result)
}

fn is_signal(msg: &Message, owner: &str, path: &str, interface: &str) -> bool {
    let h = msg.header();
    h.message_type() == Type::Signal
        && h.sender().map(|s| s.as_str()) == Some(owner)
        && h.path().map(|p| p.as_str()) == Some(path)
        && h.interface().map(|i| i.as_str()) == Some(interface)
}
fn lifecycle(msg: &Message, owner: &str, session: Option<&OwnedObjectPath>) -> Result<(), String> {
    let h = msg.header();
    if is_signal(msg, DBUS, "/org/freedesktop/DBus", DBUS)
        && h.member().map(|m| m.as_str()) == Some("NameOwnerChanged")
    {
        let (name, old, new): (String, String, String) =
            msg.body().deserialize().map_err(|e| e.to_string())?;
        if name == SERVICE && old == owner && new != owner {
            return Err("portal service owner changed or disconnected".into());
        }
    }
    if session.is_some_and(|s| is_signal(msg, owner, s.as_str(), SESSION_IFACE))
        && h.member().map(|m| m.as_str()) == Some("Closed")
    {
        return Err("shortcut session closed or permission revoked".into());
    }
    Ok(())
}
fn response_from(
    msg: &Message,
    owner: &str,
    request: &OwnedObjectPath,
) -> Option<Result<Results, String>> {
    if !is_signal(msg, owner, request.as_str(), REQUEST_IFACE)
        || msg.header().member().map(|m| m.as_str()) != Some("Response")
    {
        return None;
    }
    Some(
        msg.body()
            .deserialize::<(u32, Results)>()
            .map_err(|e| e.to_string())
            .and_then(|(code, result)| match code {
                0 => Ok(result),
                1 => Err("portal permission cancelled by user".into()),
                _ => Err(format!(
                    "portal permission denied/request failed (response {code})"
                )),
            }),
    )
}

async fn portal_call<B>(
    stream: &mut PortalStream,
    proxy: &Proxy<'_>,
    owner: &str,
    method: &str,
    body: &B,
    session: Option<&OwnedObjectPath>,
) -> Result<Results, String>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    // Keep draining while the method reply is pending: responses may arrive
    // first, including at a server-chosen path different from handle_token.
    let call = proxy.call_method(method, body);
    pin_mut!(call);
    let deadline = tokio::time::sleep(CALL_TIMEOUT);
    pin_mut!(deadline);
    let mut early = VecDeque::new();
    let request: OwnedObjectPath = loop {
        tokio::select! {
            reply = &mut call => {
                let reply=reply.map_err(|e|format!("{method}: {e}"))?;
                if reply.header().sender().map(|s|s.as_str())!=Some(owner) { return Err("portal method reply has unexpected sender".into()); }
                break reply.body().deserialize().map_err(|e|e.to_string())?;
            }
            msg = stream.messages.next() => {
                let msg=msg.ok_or("session bus disconnected")?.map_err(|e|e.to_string())?;
                stream.lifecycle(&msg,owner,session)?;
                if msg.header().message_type()==Type::Signal {
                    if early.len() >= 128 { return Err("portal response queue overflow".into()); }
                    early.push_back(msg);
                }
            }
            _ = &mut deadline => return Err(format!("{method} method reply timed out")),
        }
    };
    if !request
        .as_str()
        .starts_with(&format!("{PORTAL_PATH}/request/"))
    {
        return Err("request handle outside portal namespace".into());
    }
    let mut response = None;
    for msg in early {
        if let Some(result) = response_from(&msg, owner, &request) {
            if response.is_some() {
                return Err("duplicate portal request Response".into());
            }
            // A successful Response is the authoritative binding snapshot at
            // this position in the wire stream; earlier provisional changes
            // must not overwrite it. Later changes remain ordered after it.
            stream.changes.clear();
            response = Some(result);
        } else {
            stream.remember_change(msg, owner)?;
        }
    }
    if let Some(response) = response {
        return response;
    }
    let deadline = tokio::time::sleep(CONSENT_TIMEOUT);
    pin_mut!(deadline);
    loop {
        tokio::select! {
            msg = stream.messages.next() => {
                let msg=msg.ok_or("session bus disconnected")?.map_err(|e|e.to_string())?;
                stream.lifecycle(&msg,owner,session)?;
                if let Some(result)=response_from(&msg,owner,&request) {
                    stream.changes.clear();
                    return result;
                }
                stream.remember_change(msg, owner)?;
            }
            _ = &mut deadline => {
                if let Ok(close)=Proxy::new(proxy.connection(),owner,request.as_str(),REQUEST_IFACE).await {
                    let _=tokio::time::timeout(Duration::from_secs(2),close.call::<_,_,()>("Close",&())).await;
                }
                return Err(format!("{method} permission response timed out; restart to request again"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registered_but_unbound_and_unknown_ids_are_not_approved() {
        let value = |s: &str| OwnedValue::try_from(Value::from(s)).unwrap();
        let bound = vec![
            (
                PTT.into(),
                HashMap::from([("trigger_description".into(), value("none"))]),
            ),
            (
                CANCEL.into(),
                HashMap::from([("trigger_description".into(), value(" F9 "))]),
            ),
            (
                "other".into(),
                HashMap::from([("trigger_description".into(), value("F10"))]),
            ),
        ];
        assert_eq!(
            approved_bindings(bound).unwrap(),
            HashMap::from([(CANCEL.into(), "F9".into())])
        );
        assert!(approved_bindings(vec![
            (PTT.into(), HashMap::new()),
            (PTT.into(), HashMap::new())
        ])
        .is_err());
    }
    #[test]
    fn identity_is_stable_and_desktop_basename_only() {
        assert!(valid_app_id(APP_ID));
        for invalid in [
            "wispr-flow",
            "",
            "ai..Flow",
            "ai.wisprflow.Flow.desktop",
            "ai.wisprflow/Flow",
        ] {
            assert!(!valid_app_id(invalid));
        }
    }
    #[test]
    fn physical_suggestions_do_not_require_bindable_logical_chords() {
        assert_eq!(
            suggested_triggers(&LogicalShortcuts {
                ptt: vec![162, 91],
                cancel: vec![27]
            }),
            ("F8".into(), "F9".into())
        );
        assert_eq!(
            suggested_triggers(&LogicalShortcuts {
                ptt: vec![120],
                cancel: vec![27]
            }),
            ("F9".into(), "F8".into())
        );
        assert_eq!(
            suggested_triggers(&LogicalShortcuts {
                ptt: vec![117],
                cancel: vec![122]
            }),
            ("F6".into(), "F11".into())
        );
    }
}
