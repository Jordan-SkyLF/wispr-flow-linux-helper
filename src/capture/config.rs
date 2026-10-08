//! Read the logical shortcuts from Wispr's own settings without modifying them.
//!
//! KDE owns the physical portal triggers. These chords are only the Windows VK
//! events the app expects on its existing helper IPC channel. In particular,
//! modifier-only logical chords are valid even though KDE cannot bind them as
//! physical triggers. No second shortcut configuration is maintained here.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::keymap;

// Config contains more than shortcuts, but reading a corrupt or unexpectedly
// huge file every poll must not exhaust the helper. Never include its contents
// in an error: other prefs can contain private runtime data.
const MAX_CONFIG_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalShortcuts {
    pub ptt: Vec<u32>,
    pub cancel: Vec<u32>,
}

/// Resolve `~/.config/Wispr Flow/config.json`, honoring `XDG_CONFIG_HOME`.
pub fn config_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("Wispr Flow").join("config.json"))
}

/// Read a fresh snapshot on every poll and compare [`LogicalShortcuts`] values.
/// This also observes atomic replacement and changes with an unchanged mtime.
///
/// Missing first-run settings are pending, not invented defaults. The portal
/// worker can wait until the app writes a usable PTT chord. Invalid or removed
/// settings after capture starts require cancellation before any remapping.
pub fn read_shortcuts(path: &Path) -> Result<LogicalShortcuts, String> {
    shortcuts_from_json(&read_json(path)?)
}

/// Recover only the current Dismiss action when PTT becomes invalid or absent.
/// Use this after latching insertion off and releasing the OLD logical keys:
/// those keys may conflict with actions in the newly loaded configuration.
/// An error means cancellation is unknown; never substitute an obsolete chord.
pub fn read_cancel(path: &Path) -> Result<Vec<u32>, String> {
    cancel_from_map(shortcuts_map(&read_json(path)?)?)
}

fn read_json(path: &Path) -> Result<Value, String> {
    let file = File::open(path).map_err(|e| {
        format!(
            "cannot read Wispr shortcut settings at {}: {e}; open Wispr Flow and finish shortcut setup",
            path.display()
        )
    })?;
    if !file
        .metadata()
        .map_err(|e| format!("cannot inspect Wispr shortcut settings: {e}"))?
        .is_file()
    {
        return Err("Wispr shortcut settings must be a regular config.json file".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read Wispr shortcut settings: {e}"))?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err("Wispr config.json exceeds the 8 MiB shortcut-read limit".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("Wispr config.json is not valid JSON: {e}"))
}

fn shortcuts_map(root: &Value) -> Result<&Map<String, Value>, String> {
    root.get("prefs")
        .and_then(|v| v.get("user"))
        .and_then(|v| v.get("shortcuts"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            "Wispr has not saved prefs.user.shortcuts; finish shortcut setup in the app".into()
        })
}

fn cancel_from_map(shortcuts: &Map<String, Value>) -> Result<Vec<u32>, String> {
    // The actual app's Dismiss handler uses Escape when no Dismiss action is
    // configured (pinned Wispr 1.6.1074 keyboard handler, contract-tested by the
    // packaging repo). Do not replace an explicitly configured custom Dismiss.
    let cancel = action_chord(shortcuts, "dismiss")?.unwrap_or_else(|| vec![27]);
    for (chord, action) in shortcuts {
        if action.as_str() == Some("dismiss") {
            continue;
        }
        // The app applies parseInt to each token: malformed text such as
        // "27garbage" can become Escape and intercept our cancellation pulse.
        // Do not imitate that permissive parser or assume malformed text is
        // harmless. Fully numeric unknown OS codes and the leading -1 layered
        // sentinel can remain: neither is silently translated into our keys.
        if !numeric_or_layered_chord(chord) {
            return Err(
                "malformed competing shortcut prevents safe Dismiss; reset the invalid shortcut in the app"
                    .into(),
            );
        }
        if let Ok(keys) = parse_chord_key(chord) {
            if keys.iter().all(|vk| cancel.contains(vk)) {
                return Err(
                    "another Wispr action can intercept Dismiss; reset the conflicting shortcut in the app"
                        .into(),
                );
            }
        }
    }
    Ok(cancel)
}

fn numeric_or_layered_chord(chord: &str) -> bool {
    chord.split('+').enumerate().all(|(index, token)| {
        let token = token.trim();
        (index == 0 && token == "-1")
            || (!token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

fn shortcuts_from_json(root: &Value) -> Result<LogicalShortcuts, String> {
    let shortcuts = shortcuts_map(root)?;
    let ptt = action_chord(shortcuts, "ptt")?
        .ok_or("no PTT action in Wispr shortcut settings; set or reset push-to-talk in the app")?;
    let cancel = cancel_from_map(shortcuts)?;

    if cancel.iter().all(|vk| ptt.contains(vk)) || ptt.iter().all(|vk| cancel.contains(vk)) {
        return Err(
            "PTT and Dismiss shortcuts must each contain a key absent from the other; reset the overlapping shortcuts in Wispr Flow"
                .into(),
        );
    }

    // Exact matching precedes the app's Dismiss-with-other-keys-held fallback.
    // An unrelated action on PTT+Dismiss would therefore intercept cancellation.
    // This also catches a reassigned Escape when using the app's fallback.
    let mut combined = ptt.clone();
    combined.extend(cancel.iter().filter(|vk| !ptt.contains(vk)).copied());
    normalize_order(&mut combined);
    for (chord, action) in shortcuts {
        if matches!(action.as_str(), Some("ptt" | "dismiss")) {
            continue;
        }
        if let Ok(keys) = parse_chord_key(chord) {
            if keys == ptt || keys == cancel || keys == combined {
                return Err(
                    "another Wispr action conflicts with PTT or Dismiss cancellation; reset the conflicting shortcut in the app"
                        .into(),
                );
            }
        }
    }
    Ok(LogicalShortcuts { ptt, cancel })
}

fn action_chord(shortcuts: &Map<String, Value>, action: &str) -> Result<Option<Vec<u32>>, String> {
    let mut selected = None;
    for (chord, value) in shortcuts {
        if value.as_str() != Some(action) {
            continue;
        }
        let parsed = parse_chord_key(chord).map_err(|e| {
            format!(
                "invalid {action} shortcut: {e}; set or reset this shortcut in Wispr Flow on Linux"
            )
        })?;
        if selected.as_ref().is_some_and(|current| *current != parsed) {
            return Err(format!(
                "multiple distinct {action} shortcuts are ambiguous for portal capture; keep one {action} shortcut in Wispr Flow"
            ));
        }
        selected = Some(parsed);
    }
    Ok(selected)
}

fn parse_chord_key(key: &str) -> Result<Vec<u32>, String> {
    let mut vks = Vec::new();
    for token in key.split('+') {
        let token = token.trim();
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_digit()) {
            return Err(
                "expected complete numeric Linux VK codes; layered -1 and macOS-only bindings cannot be translated"
                    .into(),
            );
        }
        let vk = token
            .parse::<u32>()
            .map_err(|_| "shortcut VK code is out of range")?;
        if keymap::vk_to_keysym(vk).is_none() {
            return Err(format!(
                "unsupported Linux keyboard VK {vk}; mouse shortcuts and macOS Fn/Command codes are not translated"
            ));
        }
        if vks.contains(&vk) {
            return Err(format!("duplicate VK {vk} in shortcut"));
        }
        vks.push(vk);
    }
    normalize_order(&mut vks);
    Ok(vks)
}

fn normalize_order(vks: &mut [u32]) {
    // Preserve exact left/right VK identity. The app matches sets of numeric
    // codes; neither it nor this helper translates generic modifier aliases.
    // Press modifiers first, regular keys last; the state machine releases in
    // reverse. A deterministic order also makes semantic config comparison work.
    vks.sort_unstable_by_key(|vk| {
        let rank = match vk {
            162 | 163 => 0,
            91 | 92 => 1,
            164 | 165 => 2,
            160 | 161 => 3,
            _ => 4,
        };
        (rank, *vk)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn settings(shortcuts: Value) -> Value {
        json!({"prefs": {"user": {"shortcuts": shortcuts}}})
    }

    #[test]
    fn modifier_only_logical_ptt_and_custom_cancel_use_app_settings() {
        let result = shortcuts_from_json(&settings(json!({
            "91+162": "ptt", "27+162": "dismiss", "-1+162+86": "paste_last_text"
        })))
        .unwrap();
        assert_eq!(result.ptt, vec![162, 91]);
        assert_eq!(result.cancel, vec![162, 27]);
    }

    #[test]
    fn missing_dismiss_uses_the_apps_escape_fallback() {
        let result = shortcuts_from_json(&settings(json!({"162+91": "ptt"}))).unwrap();
        assert_eq!(result.cancel, vec![27]);
    }

    #[test]
    fn missing_fresh_or_removed_ptt_never_invents_a_default() {
        for root in [
            json!({}),
            json!({"prefs": {}}),
            json!({"prefs": {"user": {}}}),
            settings(Value::Null),
            settings(json!([])),
            settings(json!({})),
            settings(json!({"27": "dismiss"})),
        ] {
            assert!(shortcuts_from_json(&root).is_err(), "{root}");
        }
    }

    #[test]
    fn malformed_or_unsupported_tokens_are_not_silently_dropped() {
        for key in [
            "",
            "162+",
            "+162",
            "162++91",
            "162+oops+91",
            "162.0+91",
            "-1+162+91",
            "-2+162+91",
            "0",
            "63",
            "4098",
            "4294967296",
            "18446744073709551616",
            "16+91",
            "17+91",
            "18+91",
            "162+162",
        ] {
            assert!(parse_chord_key(key).is_err(), "accepted {key}");
        }
    }

    #[test]
    fn left_and_right_codes_are_preserved_without_guessing_mac_semantics() {
        assert_eq!(parse_chord_key("120+92+163").unwrap(), vec![163, 92, 120]);
        // 55 is both a valid Windows digit key and a macOS Command keycode.
        // No platform provenance exists in this config; silently translating it
        // would change the configured action. Known invalid Fn=63 is rejected.
        assert_eq!(parse_chord_key("55+162").unwrap(), vec![162, 55]);
        assert_eq!(parse_chord_key(" 162 + 119 ").unwrap(), vec![162, 119]);
    }

    #[test]
    fn distinct_duplicate_actions_are_rejected_instead_of_choosing_map_order() {
        for shortcuts in [
            json!({"162+91": "ptt", "162+119": "ptt", "27": "dismiss"}),
            json!({"162+91": "ptt", "27": "dismiss", "162+27": "dismiss"}),
        ] {
            assert!(shortcuts_from_json(&settings(shortcuts))
                .unwrap_err()
                .contains("multiple distinct"));
        }
    }

    #[test]
    fn equivalent_chord_order_is_not_a_new_logical_configuration() {
        let first = shortcuts_from_json(&settings(json!({"162+91": "ptt"}))).unwrap();
        let second = shortcuts_from_json(&settings(json!({
            "91+162": "ptt", "162+91": "ptt", "162+164+32": "lens"
        })))
        .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn invalid_custom_dismiss_never_falls_back_to_escape() {
        assert!(shortcuts_from_json(&settings(json!({
            "162+91": "ptt", "-1+27": "dismiss"
        })))
        .unwrap_err()
        .contains("invalid dismiss"));
    }

    #[test]
    fn overlapping_actions_and_intercepted_cancellation_are_rejected() {
        for shortcuts in [
            json!({"162+91": "ptt", "162": "dismiss"}),
            json!({"162+91": "ptt", "162+91+27": "dismiss"}),
            json!({"162+91": "ptt", "27": "lens"}),
            json!({"162+91": "ptt", "27": "dismiss", "27+162+91": "lens"}),
            json!({"162+91": "ptt", "91+162": "lens", "27": "dismiss"}),
        ] {
            assert!(shortcuts_from_json(&settings(shortcuts)).is_err());
        }
    }

    struct TempConfig(PathBuf);
    impl TempConfig {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "wispr-hybrid-config-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            Self(dir.join("config.json"))
        }
    }
    impl Drop for TempConfig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    #[test]
    fn reading_observes_first_run_and_same_mtime_changes_without_writing_settings() {
        let config = TempConfig::new();
        assert!(read_shortcuts(&config.0).is_err());
        assert!(!config.0.exists());
        let first_bytes = serde_json::to_vec(&settings(json!({"162+91": "ptt"}))).unwrap();
        std::fs::write(&config.0, &first_bytes).unwrap();
        let first = read_shortcuts(&config.0).unwrap();
        assert_eq!(std::fs::read(&config.0).unwrap(), first_bytes);
        let mtime = std::fs::metadata(&config.0).unwrap().modified().unwrap();
        let second_bytes = serde_json::to_vec(&settings(json!({"163+92": "ptt"}))).unwrap();
        assert_eq!(first_bytes.len(), second_bytes.len());
        std::fs::write(&config.0, &second_bytes).unwrap();
        File::open(&config.0)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(mtime))
            .unwrap();
        assert_eq!(
            std::fs::metadata(&config.0).unwrap().modified().unwrap(),
            mtime
        );
        assert_ne!(first, read_shortcuts(&config.0).unwrap());
        assert_eq!(std::fs::read(&config.0).unwrap(), second_bytes);
    }

    #[test]
    fn corrupt_and_oversized_files_do_not_enable_capture() {
        let config = TempConfig::new();
        std::fs::write(&config.0, b"{not JSON").unwrap();
        assert!(read_shortcuts(&config.0)
            .unwrap_err()
            .contains("valid JSON"));
        File::create(&config.0)
            .unwrap()
            .set_len(MAX_CONFIG_BYTES + 1)
            .unwrap();
        assert!(read_shortcuts(&config.0).unwrap_err().contains("8 MiB"));
    }

    #[test]
    fn current_cancel_is_available_even_if_ptt_is_missing_or_invalid() {
        let config = TempConfig::new();
        for shortcuts in [
            json!({"164+27": "dismiss"}),
            json!({"63": "ptt", "164+27": "dismiss"}),
            json!({"-1": "ptt", "164+27": "dismiss"}),
        ] {
            let bytes = serde_json::to_vec(&settings(shortcuts)).unwrap();
            std::fs::write(&config.0, &bytes).unwrap();
            assert!(read_shortcuts(&config.0).is_err());
            assert_eq!(read_cancel(&config.0).unwrap(), vec![164, 27]);
            assert_eq!(std::fs::read(&config.0).unwrap(), bytes);
        }
    }

    #[test]
    fn standalone_cancel_rejects_ambiguity_and_competing_actions() {
        let config = TempConfig::new();
        for shortcuts in [
            json!({"63": "ptt", "-1+27": "dismiss"}),
            json!({"27": "dismiss", "164+27": "dismiss"}),
            json!({"63": "ptt", "27": "popo"}),
            json!({"164+27": "dismiss", "164": "popo"}),
            json!({"164+27": "dismiss", "27+164": "popo"}),
        ] {
            std::fs::write(&config.0, serde_json::to_vec(&settings(shortcuts)).unwrap()).unwrap();
            assert!(read_cancel(&config.0).is_err());
        }
        std::fs::write(&config.0, b"{invalid").unwrap();
        assert!(read_cancel(&config.0).is_err());
    }

    #[test]
    fn standalone_cancel_does_not_assume_old_held_keys_are_still_safe() {
        let config = TempConfig::new();
        std::fs::write(
            &config.0,
            serde_json::to_vec(&settings(json!({
                "63": "ptt", "164+27": "dismiss", "162+91+164+27": "popo"
            })))
            .unwrap(),
        )
        .unwrap();
        // Safe only after the OLD 162+91 keys are released. The root portal
        // worker performs that release before pulsing a replacement Dismiss.
        assert_eq!(read_cancel(&config.0).unwrap(), vec![164, 27]);
    }

    #[test]
    fn malformed_competing_text_cannot_hide_an_escape_binding() {
        let config = TempConfig::new();
        std::fs::write(
            &config.0,
            serde_json::to_vec(&settings(json!({
                "162+91": "ptt", "27garbage": "popo"
            })))
            .unwrap(),
        )
        .unwrap();
        assert!(read_cancel(&config.0)
            .unwrap_err()
            .contains("malformed competing shortcut"));
        std::fs::write(
            &config.0,
            serde_json::to_vec(&settings(json!({
                "162+91": "ptt", "27": "dismiss", "63": "lens", "4098": "popo",
                "-1+162+86": "paste_last_text"
            })))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(read_cancel(&config.0).unwrap(), vec![27]);
        assert!(read_shortcuts(&config.0).is_ok());
    }
}
