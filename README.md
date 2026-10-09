# Wispr Flow Linux helper — portal hybrid

This private successor branch adapts the upstream asynchronous GlobalShortcuts
implementation for KDE Plasma on Wayland. It retains Wispr's stdin/fd-3 helper
protocol, its saved logical shortcuts, Tokio/zbus, the uinput insertion backend,
and XInput2 capture on true X11 sessions. It selectively imports lifecycle,
physical-input protections, and failed-device recovery from the earlier
`feature/portal-shortcuts` experiment.

The target is a candidate for CachyOS desktop acceptance. Automated protocol and
application-handler tests do not establish microphone, transcription, clipboard,
or desktop insertion behavior. Earlier upstream live tests do not validate this
hybrid. The previous feature branch remains a rollback option.

The packaging repository contains the recovered
[IPC contract](https://github.com/Jordan-SkyLF/wispr-flow-linux/blob/feature/portal-hybrid/docs/reference/ipc-contract.md),
[build and configuration guide](https://github.com/Jordan-SkyLF/wispr-flow-linux/blob/feature/portal-hybrid/docs/portal-hybrid.md),
and the candidate's exact source and validation handoff. No proprietary Wispr
application code is included in this repository.

## Shortcut ownership

Wispr's saved PTT and Dismiss bindings are the **logical** actions.
The helper reads the application's existing settings and emits the same Windows
VK `KeypressEvent` messages as the original helpers. KDE owns the **physical**
shortcuts, approval, and persistence.

For example, KDE can approve F8 for push-to-talk while the helper emits Wispr's
Ctrl+Meta logical chord. Modifier-only logical chords are supported; they are
not offered as modifier-only portal triggers. A suitable existing bare function
key is used as an initial suggestion where possible; otherwise the suggestions
are F8 and F9. They are editable suggestions, not fixed requirements.

Logical changes are read before activation, after the consent dialog, on the
application's `UpdateShortcuts` command, and by periodic content comparison.
Idle logical changes do not overwrite KDE's physical choices. Changes during
recording or possible processing cancel and disable capture until restart.

The portal cannot observe arbitrary keys for Wispr's in-app shortcut recorder.
Use KDE's approved-shortcut controls to change physical triggers. Saved, synced,
or reset logical settings still propagate. Missing or invalid settings keep
capture and insertion off with an actionable diagnostic; the helper does not
translate incompatible macOS codes or substitute an unrelated cancellation
action. Fresh users must finish Wispr setup so it writes valid settings.

## Portal status and saved shortcut changes

The helper emits full `PortalShortcutStatus` snapshots through the existing
stdin/fd-3 protocol's `HelperAPIRequest` envelope. Its `payload` contains
`version: 1`, `mode: "portal"`, an overall `state` (`pending`, `ready`, `error`,
`stopped`), `actions` for the fixed `ptt` and `cancel` IDs, and nullable `error`.
Each action has `id`, `state` (`pending`, `bound`, `unbound`, `error`), nullable
`trigger`, and nullable `error`. Trigger descriptions and errors are limited to
512 UTF-8 bytes each. Every snapshot replaces previous status. A bound trigger
reports portal approval; physical activation, recording, and insertion still
require desktop acceptance. Failure and shutdown clear previous bound triggers.

An idle change to valid saved PTT or Dismiss logical chords suspends insertion,
clears old approval, closes the old portal session, and binds a new session.
Replacement sessions omit physical trigger suggestions, leaving persisted
choices to the portal. An unchanged `UpdateShortcuts`, an unrelated preference,
or an unrelated action that does not invalidate cancellation does not rebind.
The supported portal action set remains PTT and Cancel; Dismiss's documented
Escape fallback still supplies the logical Cancel action. Missing or invalid
PTT and unsafe cancellation mappings after approval are terminal configuration
faults. Before first approval, invalid or missing saved settings remain pending
with the reason shown beside the actions; saving valid settings automatically
continues setup and clears that reason before requesting consent.

Changes during recording or possible processing cancel safely and require an
explicit Wispr Flow restart. Refresh denial, failed session cleanup, service or
backend loss also latch capture and insertion off until restart. Late old-session
signals cannot restore approval or synthesize keys. There is no raw-input fallback.

## Capture configuration

| `WISPR_CAPTURE` | Behavior |
| --- | --- |
| unset or `auto` | Portal on Wayland; XInput2 on true X11; no capture headlessly. |
| `portal` | Portal capture only. Failure never selects evdev. |
| `x11` | XInput2, accepted only on a true X11 session. |
| `evdev` | Explicit legacy physical keyboard monitoring, including paste-time modifier scans. Requires separately arranged device access. |
| `none` | No shortcut capture. Intended for diagnostics or deliberate manual operation. |

Unknown or explicitly empty modes disable capture and insertion. An XWayland
`DISPLAY` does not qualify as a true X11 session. The deprecated
`WISPR_KEY_CAPTURE` variable is recognized only when `WISPR_CAPTURE` is absent;
its old `xinput` spelling maps to `x11`. The canonical variable wins if both
exist. Remove obsolete `WISPR_PORTAL_SHORTCUTS` JSON; it is ignored rather than
maintained as a second logical configuration.

`WISPR_PORTAL_APP_ID` defaults to `ai.wisprflow.Flow`, paired with
`ai.wisprflow.Flow.desktop` in packages and AppImages. Host Registry registration
uses the same D-Bus connection as the shortcut session. A missing Registry
interface produces a warning to launch from the installed desktop entry and
verify identity. Other registration failures disable capture. Changing this ID
can change KDE's stored permission and shortcut association.

## Failure behavior and security boundary

Portal messages are checked against the pinned unique service owner, interface,
path, session, and approved action ID. Requests use the returned handle path.
An ordered stream and small action state machine handle duplicate activations,
zero/equal/non-monotonic timestamps, early responses, and binding changes.
An empty or `none` trigger description is not an active binding. Registration,
the first received activation, and actual dictation are separate observations.

On KDE, the backend service owner is pinned as well as the frontend owner.
Backend replacement cancels even when the frontend retains its session and
never sends a release or closure signal. Only authenticated bus owner-change
signals can trigger this protection.

Denial, owner replacement, disconnection, session closure, active binding loss,
or a change during possible recording/processing closes the insertion gate
before cleanup. For unchanged settings, logical Dismiss precedes PTT release.
After a remap, old held keys must be released before the new valid Dismiss can
work. The pinned application can briefly enter its stopping/transcription path
in that exceptional case; insertion remains blocked. If a safe current Dismiss
cannot be established, cleanup releases keys and requests cancellation in the
app UI instead of pressing an obsolete or guessed shortcut. Cancellation is not
confirmed in that case.

Cleanup is idempotent. A failed portal does not reconnect or regain insertion
permission within the process; fix the cause and explicitly restart Wispr.
IPC readiness remains responsive. A missing portal release cannot be discovered
immediately without raw monitoring: a five-minute continuous held-action limit
cancels and disables capture. This limit applies to a continuously held trigger,
not ordinary released-key hands-free recording. Already delivered OS input or
paste cannot be retracted.

Default Wayland capture **and text insertion** do not open `/dev/input/event*`.
The legacy modifier scanner is reachable only with explicit `evdev` capture.
Packaging grants no physical input access by default and requires no root app,
privileged daemon, or broad `input` group membership. Injection still uses
`/dev/uinput`: every process with the same access can synthesize input. This is
not an application-exclusive permission or complete isolation boundary.

Uinput tracks its synthetic down keys. An uncertain key or synchronization write
attempts all outstanding releases, destroys and closes the virtual device, and
prevents reuse. Insertion checks the portal gate before dispatch and before each
new key-down; releases remain permitted. Physical modifiers held by the user
cannot be inspected or neutralized in default portal mode. Release unrelated
physical modifiers when testing paste.

## Build and automated verification

Use a Rust toolchain that supports the committed lockfile:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

`x11rb` speaks X11 directly and requires no libX11 development headers. Desktop
runtime requirements include the compositor/portal, writable `/dev/uinput`,
and clipboard tools as documented in the packaging guide. Building a helper
changes no device permissions and does not install it into Wispr.

Real transport tests use a private `dbus-daemon` with a mock portal, the compiled
helper, actual framing, and temporary Wispr settings:

```bash
cargo build --locked
python3 -m venv .venv
.venv/bin/pip install -r tests/requirements-portal.txt
.venv/bin/python tests/test_portal_integration.py
# To test the release binary:
WISPR_TEST_HELPER="$PWD/target/release/wispr-flow-linux-helper" \
  .venv/bin/python tests/test_portal_integration.py
```

If the environment forbids Unix sockets, set `WISPR_TEST_BUS_TRANSPORT=tcp`
explicitly to use loopback-only anonymous test authentication. That fallback
does not verify Unix peer credentials or FD passing. The suite records missing
ptrace or child `/proc` access as skips; they are not passing syscall audits.
The libc open interposer includes an explicit evdev positive control.

The packaging repository separately exercises source-independent fixtures and
the actual pinned Wispr 1.6.1074 keyboard/state code without committing that
code. Packaging verifies immutable source, patch, lockfile, complete source
tree, and built binary SHA-256. A helper's version string is not provenance.

## Desktop acceptance still required

On the target CachyOS KDE/Wayland desktop, verify fresh consent and editable
bindings, background PTT and microphone transcription, native Wayland and
XWayland insertion, cancellation both while recording and while processing,
logical and KDE physical shortcut changes, identity and persistence after
restart, portal denial/closure/restart, and lock/suspend behavior. Audit physical
device access through idle, recording, and paste; check for stuck modifiers and
unintended insertion. Run the candidate separately before replacing an installed
application.

## Legal

Clean-room implementation of the recovered helper contract, under the
[Unlicense](UNLICENSE). Wispr Flow itself remains subject to its own terms.
