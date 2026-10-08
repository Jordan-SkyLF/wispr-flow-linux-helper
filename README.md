# wispr-flow-linux-helper

Clean-room Linux helper for the existing Wispr Flow Electron application. It
speaks the app's helper IPC contract on stdin and fd 3 and supplies Linux desktop
integration. It does not implement speech recognition or replace the application.

**This feature/portal-shortcuts branch is experimental.** As of 2026-10-08,
the Rust implementation and a real-process/private-D-Bus protocol suite have
been exercised in a headless development container. This branch has **not**
passed physical CachyOS/KDE Plasma Wayland acceptance, actual speech recognition,
or text insertion into native Wayland and XWayland applications. Historical
upstream claims of live-validated PasteText describe upstream work; they are
not validation of this feature branch.

The helper was split from the upstream
[wispr-flow-linux repository](https://github.com/wispr-flow-linux/wispr-flow-linux)
into [wispr-flow-linux/helper](https://github.com/wispr-flow-linux/helper).
Upstream tagged releases publish prebuilt helpers. The corresponding security
branch of [Jordan's packaging fork](https://github.com/Jordan-SkyLF/wispr-flow-linux/tree/feature/portal-shortcuts)
builds its pinned modified helper source, so an unchanged upstream binary must
not be substituted. See that branch's
[handoff and acceptance procedure](https://github.com/Jordan-SkyLF/wispr-flow-linux/blob/feature/portal-shortcuts/docs/portal-shortcuts-handoff.md).

**Contract is the source of truth:**
[docs/reference/ipc-contract.md](https://github.com/Jordan-SkyLF/wispr-flow-linux/blob/feature/portal-shortcuts/docs/reference/ipc-contract.md),
keycodes.json, and commands.json live in the packaging repository. The
contract was recovered from the shipped Electron bundle.

## Portal shortcut capture

The default capture backend is the
[XDG GlobalShortcuts portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html).
It receives activation and deactivation of approved actions even while Wispr
is unfocused. It does not receive arbitrary keyboard events.

| Physical shortcut offered to KDE | Action | Logical Windows VK codes sent only to Wispr IPC |
|---|---|---|
| Hold **F8**, release to stop | dictate | Press 162, 91; release 91, 162 |
| **F9** | dismiss | Press and release 27 (Escape) |

The logical dictate chord matches Wispr's default left-Control + left-Windows
binding. These logical events are **not injected into the focused application**;
they control Wispr through its existing KeypressEvent contract. Text insertion
uses the separate injection backend described below.

F8 avoids physically held shortcut modifiers affecting the later paste. Release
other physical modifiers before insertion. KDE controls the actual accepted
shortcuts and may preserve previously configured keys instead of adopting new
preferred defaults. Change physical keys in KDE System Settings → Shortcuts.
Use a shortcut with a regular key for push-to-talk; modifier-only shortcuts
can activate on release in KDE and are unsuitable for this purpose.

The helper registers its desktop identity on its portal D-Bus connection,
creates a session, and calls BindShortcuts exactly once for that new session.
It accepts only signals from that portal owner for the current session and
configured, approved action IDs. Repeated activations are deduplicated; release
events undo logical keys in reverse order, retaining shared modifiers while
another active action still uses them. KDE currently supplies zero timestamps,
so delivery order and active action state determine transitions.
The KDE behavior was checked against its
[portal implementation](https://github.com/KDE/xdg-desktop-portal-kde/blob/f26e64b73e000de5027efa0fd3f6d26c855f4868/src/globalshortcuts.cpp)
and [shortcut event implementation](https://github.com/KDE/kglobalacceld/blob/5b7f39b88d33877aeecaf9e73e80cbf755c14f12/src/component.cpp).

CheckStaleKeys reports the helper's logical approved-chord state. It does not
query physical keys. Portal setup and permission dialogs run off the stdin
command thread, so IsReady still receives its prompt ACK. An ACK means the
helper is responsive; it does not prove that shortcut capture or insertion is
available.

When an established portal session closes, loses its service/bus connection,
receives a malformed relevant signal, or changes bindings, the helper sends
the configured logical Dismiss chord before clearing held keys. This also
cancels work after a physical key has been released, when Wispr may still be
processing or using hands-free capture. Setup denial creates no such key events.
There is no automatic permission retry or raw-input fallback. After a portal or
bus failure, restart Wispr to establish a new session.

### Configuration

Set these variables in the environment that launches Wispr, then restart the
application. Unregistered physical keys cannot be observed by the app's
shortcut recorder in portal mode.

| Variable | Meaning |
|---|---|
| WISPR_KEY_CAPTURE | Defaults to portal. none disables capture. Explicit legacy xinput is limited to true X11 sessions; explicit evdev reads raw keyboard devices. Neither is a fallback. |
| WISPR_PORTAL_APP_ID | Defaults to wispr-flow, matching wispr-flow.desktop. AppImage packaging sets ai.wisprflow.WisprFlow. The corresponding .desktop file must be discoverable by the host portal. |
| WISPR_PORTAL_SHORTCUTS | Optional JSON array replacing the default action definitions. Each needs id, description, preferred_trigger, and keys (Windows VK integers). |
| RUST_LOG | Diagnostic logging on stderr, for example debug. |

For example, this retains the default logical app bindings while requesting
different physical function keys:

~~~bash
export WISPR_PORTAL_SHORTCUTS='[{"id":"dictate","description":"Wispr Flow: hold to dictate","preferred_trigger":"F10","keys":[162,91]},{"id":"dismiss","description":"Wispr Flow: cancel dictation","preferred_trigger":"F11","keys":[27]}]'
~~~

The preferred_trigger uses the
[XDG shortcut syntax](https://specifications.freedesktop.org/shortcuts/latest/),
for example F8 or CTRL+LOGO+d. KDE may keep an existing assignment; edit that
assignment in System Settings. If Wispr's logical bindings were customized,
the keys arrays must match them. Keep an action named dismiss for the
configured logical cancel chord; that chord is also retained for fault cleanup
if its separate physical shortcut is not approved. Without a dismiss
definition, cleanup uses Wispr's default Escape (27).

The [host Registry API](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.host.portal.Registry.html)
requires registration before portal operations and matches the application ID
to a desktop-file basename. A missing desktop file is an error. Older portals
without the Registry interface use their existing desktop-derived identity,
with a warning that shortcut persistence may be unreliable. Other registration
failures disable capture. A registered host application ID is an identity hint,
not isolation from other processes running as the same user.

## Permissions and remaining limitations

**Default portal mode never opens /dev/input/event*, including during paste.**
The historical physical held-modifier snapshot in the uinput backend is gated
behind explicit WISPR_KEY_CAPTURE=evdev. Normal use needs neither membership
in the input group nor blanket physical keyboard device access. This package
does not provision those permissions. Existing permission grants from an older
installation must be audited separately; installing the helper cannot remove
an account's unrelated group memberships or ACLs.

Wayland insertion still uses an **in-process /dev/uinput virtual keyboard**
and the clipboard. It creates no privileged daemon. Access to
[/dev/uinput](https://docs.kernel.org/input/uinput.html) allows synthetic input,
which is a distinct capability from reading physical keyboard events. A logind
uaccess ACL grants the active user access; it is not exclusive to Wispr. Other
processes running with that access can create synthetic input devices too.
Clipboard access, accessibility reads, and the existing KWin scripting bridge
remain separate parts of the upstream integration.

On a uinput write or synchronization error, the helper attempts outstanding key
releases, destroys the virtual device, and disables further use of that device
until restart. Automated tests inject failures into the emission logic; actual
compositor behavior after device removal still requires desktop acceptance.
Because portal mode cannot read physical modifier state, physically held
Control/Alt/Shift/Meta keys can affect a paste. The F8 default reduces this risk
without adding physical keyboard access.

KDE's
[RemoteDesktop portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html)
and libei provide a possible future insertion path with compositor consent.
That path has not been implemented or demonstrated in this branch. Retaining
uinput avoids making an unverified insertion replacement a prerequisite for
the keyboard-reading improvement. Native Wayland, XWayland, Unicode, layout,
consent, and recovery behavior must be proven before changing that backend.

## Build and automated checks

Use an installed Rust toolchain and keep Cargo.lock. No build step needs root
or installation of the helper on the host:

~~~bash
cargo build --locked
cargo test --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
~~~

The release binary is target/release/wispr-flow-linux-helper.
x11rb, zbus, and the Wayland client are Rust libraries; no libxcb/libX11 or
libdbus development headers are needed. Runtime clipboard and desktop
requirements are handled by the packaging repository.

The native tests cover framing, logical chord state, cancellation, repeat
suppression, configuration validation, modifier-read gating, and uinput
emission cleanup at failure boundaries.

For protocol integration tests, install the **test-only** dependency in a
virtual environment. This adds no application runtime dependency:

~~~bash
python3 -m venv /tmp/wispr-portal-tests
/tmp/wispr-portal-tests/bin/python -m pip install -r tests/requirements-portal.txt
/tmp/wispr-portal-tests/bin/python tests/test_portal_integration.py
WISPR_TEST_HELPER="$PWD/target/release/wispr-flow-linux-helper" /tmp/wispr-portal-tests/bin/python tests/test_portal_integration.py
~~~

The suite runs the compiled helper on real stdin/fd-3 pipes against a real,
private dbus-daemon with a mock portal. It checks immediate and delayed request
responses, registration and AppImage identity, new-session binding on restart,
permission cancellation, partial approvals, reconfiguration, sender/session
filtering, press/release and dismissal, bus/service/session loss, malformed
signals, and missing portals. Audio, desktop key handling, speech recognition,
clipboard, and insertion are outside this suite.

It also compiles tests/input_open_audit.c locally when cc is available. This
test-only interposer logs and denies libc attempts to open /dev/input, with
positive controls using Python and the explicit legacy helper path. Default
capture must produce no attempts. This is a libc-level audit, not a production
sandbox or proof about direct syscalls. Separate /proc and strace checks run
where permitted.

This development container forbids Unix sockets, so its recorded runs use:

~~~bash
WISPR_TEST_BUS_TRANSPORT=tcp /tmp/wispr-portal-tests/bin/python tests/test_portal_integration.py
~~~

That selects a loopback-only private test bus with anonymous authentication,
without service activation directories. It does not exercise Unix peer
authentication or file-descriptor passing. Two checks are explicitly skipped
here: the container does not expose the child's /proc descriptors and denies
ptrace, preventing the corresponding descriptor and syscall audits. Neither
skip is counted as a pass.

Recorded validation on 2026-10-08:

| Check | Result |
|---|---|
| Native Rust unit tests | 30 passed |
| Rust formatting and Clippy with warnings denied | Passed |
| Debug helper process + private-D-Bus integration | 32 tests: 30 passed, 2 restricted-container skips |
| Libc input-open audit and positive controls | Passed within the integration suite |
| Physical KDE shortcuts, speech recognition and insertion | Not tested; acceptance required |

The formatting, Clippy and native checks were run with CARGO_INCREMENTAL=0 to
avoid this container's incremental-cache filesystem limitation. The protocol
results above used WISPR_TEST_BUS_TRANSPORT=tcp as described above.

## Inherited OS integration

The following describes the retained upstream implementations, not a fresh
physical acceptance result for this branch:

| Command | X11 implementation | Wayland implementation |
|---|---|---|
| IsReady → ACK | Handshake and keepalive | Handshake and keepalive |
| PasteText | xclip/xsel clipboard + XTEST Ctrl+V | In-process text/plain + text/html clipboard + uinput Ctrl+V |
| SimulateKeyPress | VK → keysym → keycode + XTEST | VK → evdev code + uinput chord; physical snapshots only in explicit legacy evdev mode |
| GetActiveAppInfo / GetAppInfo | Native _NET_* information, AT-SPI fallback | KWin bridge on KDE; GNOME extension and AT-SPI providers retained |
| GetRunningApps | _NET_CLIENT_LIST | Provider-dependent; KDE walks workspace.windowList |
| SetFocusChangeDetectorState | Provider-backed focus events | Gated, deduplicated provider events on fd 3 |
| GetSelectedTextViaCopy | AT-SPI selection, then Ctrl+C copy-probe | AT-SPI selection, then Ctrl+C copy-probe |
| GetAccessibilityStatus | Reports backend connection status | Reports initialized uinput backend; does not prove full dictation |
| Unsupported commands | ACK no-op | ACK no-op |

Injection selection is separate from shortcut capture. A usable Wayland/uinput
backend is preferred, then X11/XWayland insertion where available, then a no-op
stub. XTEST cannot insert into native Wayland fields. This inherited insertion
fallback does not enable XInput2 or evdev capture.

Unhandled commands are ACK'd as safe no-ops so the existing app remains
responsive. See src/main.rs dispatch; successful handshakes alone must not be
reported as successful OS integration.

## Desktop smoke tests

test_harness.py mimics Electron's four-pipe spawn and sends an IPC conversation:

~~~bash
python3 test_harness.py target/debug/wispr-flow-linux-helper
~~~

Expected responses include ACK, active-app information, running applications,
and accessibility status. Content depends on the desktop and available backend.

live_inject_test.py performs **real clipboard and synthetic key operations** in
a focused editor. Run it manually on the acceptance desktop when ready:

~~~bash
python3 live_inject_test.py target/release/wispr-flow-linux-helper
~~~

It launches Kate, pastes a marker, replaces the clipboard with a sentinel, and
uses Ctrl+A/Ctrl+C for readback. Its inherited automated readback has a
clipboard-owner race that can report a false negative; verify the editor
contents as well. This is a helper insertion smoke test, not speech recognition
or application-level push-to-talk acceptance. The packaging handoff contains the
full native Wayland/XWayland, focus, restart, cancellation, permission, and
failure acceptance procedure.

## Wiring and layout

The existing packaging patch adds Linux to the Electron helper-path resolver
and stages this binary under resources/Release/. Electron launches it with
stdio:["pipe","pipe","pipe","pipe"], preserving stdin commands and fd-3
events. The patched application's dictation pipeline remains in place.

| Path | Role |
|---|---|
| src/main.rs, src/proto.rs | Command dispatch, framing, fd-3 responses |
| src/capture/portal.rs | Default portal connection, registration, session and request lifecycle |
| src/capture/portal_state.rs | Logical key state, repeat suppression and fault cancellation |
| src/capture/evdev.rs, src/capture/xinput.rs | Explicit legacy capture implementations |
| src/backend/uinput.rs, src/backend/wayland.rs | Wayland insertion and failure cleanup |
| src/backend/wl_clipboard.rs | In-process clipboard offers |
| src/backend/x11.rs | X11 insertion, clipboard and app information |
| src/backend/kwin.rs, src/backend/gnome.rs, src/backend/atspi_app.rs | Existing active-app and focus providers |
| src/backend/atspi_sel.rs | Accessibility selection reads |
| tests/test_portal_integration.py | Real helper process and private-D-Bus protocol checks |

## Remaining work

The next gate is physical KDE acceptance of the integrated package: actual
hold-to-talk, release, cancellation, speech transcription and insertion,
background operation, persisted shortcuts, and recovery after desktop faults.
The goal is a small, functional improvement to the existing port.

Existing upstream follow-ups remain separate: native X11 focus tracking,
in-process X11 clipboard ownership and text/html, paste clipboard restoration,
and codingCliAgent detection. KWin/GNOME app information and AT-SPI selection
implementations already exist; this branch does not reimplement them. A portal
replacement for uinput is a separate future change requiring functional proof.

## Legal

Clean-room reimplementation against a recovered IPC contract; ships no Wispr
Flow proprietary code, and is released into the public domain under the
[Unlicense](UNLICENSE). The app itself remains under its own terms; see the
[upstream legal posture](https://github.com/wispr-flow-linux/wispr-flow-linux#legal-posture).
