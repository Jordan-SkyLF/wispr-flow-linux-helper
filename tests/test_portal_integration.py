#!/usr/bin/env python3
"""Exercise a compiled helper against a real, private D-Bus daemon.

Run after ``cargo build --locked``:
    python3 -m pip install -r tests/requirements-portal.txt
    python3 tests/test_portal_integration.py

Set WISPR_TEST_HELPER to an absolute binary path to test a release build.
Where a container forbids Unix sockets, WISPR_TEST_BUS_TRANSPORT=tcp runs the
same messages on a loopback-only bus with anonymous test authentication.
That mode does not test Unix peer authentication or file-descriptor passing.
The portal implementation is mocked; the D-Bus transport, helper process,
stdin/fd-3 framing, settings reads and state transitions are real. No desktop, microphone,
speech recognition, physical key press, clipboard or text insertion is tested.
The private bus has no service activation directories and the helper inherits
no display/session or user configuration. Nothing is installed or privileged.
"""

import asyncio
import contextlib
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time
import unittest

from dbus_next import Message, MessageType, Variant
from dbus_next.aio import MessageBus
from dbus_next.auth import AuthAnnonymous


SERVICE = "org.freedesktop.portal.Desktop"
KDE_SERVICE = "org.freedesktop.impl.portal.desktop.kde"
PATH = "/org/freedesktop/portal/desktop"
GLOBAL = "org.freedesktop.portal.GlobalShortcuts"
REGISTRY = "org.freedesktop.host.portal.Registry"
REQUEST = "org.freedesktop.portal.Request"
SESSION = "org.freedesktop.portal.Session"
PROPERTIES = "org.freedesktop.DBus.Properties"
BINARY = Path(os.environ.get(
    "WISPR_TEST_HELPER",
    Path(__file__).resolve().parents[1]
    / "target/debug/wispr-flow-linux-helper",
)).resolve()
TIMEOUT = 5.0
FAULT_KEYS = [
    (162, "key_event_press"), (91, "key_event_press"),
    (27, "key_event_press"), (27, "key_event_release"),
    (91, "key_event_release"), (162, "key_event_release"),
]


async def connect_bus(address):
    options = {"auth": AuthAnnonymous()} if address.startswith("tcp:") else {}
    return await MessageBus(bus_address=address, **options).connect()


async def disconnect_bus(bus):
    bus.disconnect()
    with contextlib.suppress(Exception):
        await bus.wait_for_disconnect()
    # dbus-next 0.2.3 shuts down, but does not close, these descriptors.
    # Close them after its reader has stopped to keep the fixture isolated.
    bus._stream.close()
    bus._sock.close()


def frame(envelope):
    body = json.dumps(envelope, indent=2)
    return body.replace("+", "+1").replace("|", "+2").encode() + b"|"


class MockPortal:
    """A spec-shaped service with controllable replies and adversarial signals."""

    def __init__(self, address, **options):
        self.address = address
        self.options = options
        self.bus = None
        self.calls = []
        self.sessions = []
        self.registered = {}
        self.bind_counts = {}
        self.closed = []
        self.errors = []
        self.pending = []
        self.tasks = []
        self.changed = asyncio.Event()

    async def start(self):
        self.bus = await connect_bus(self.address)
        self.bus.add_message_handler(self.handle)
        await self.bus.request_name(SERVICE)
        return self

    async def stop(self):
        if self.tasks:
            await asyncio.gather(*self.tasks)
        if self.bus is not None:
            await disconnect_bus(self.bus)

    def handle(self, msg):
        if msg.message_type != MessageType.METHOD_CALL:
            return None
        if msg.path != PATH and not msg.path.startswith(PATH + "/"):
            return None
        self.calls.append(msg)
        self.changed.set()
        try:
            return self.dispatch(msg)
        except Exception as error:
            self.errors.append(repr(error))
            return Message.new_error(
                msg, "org.freedesktop.DBus.Error.Failed", repr(error)
            )

    def dispatch(self, msg):
        if msg.interface == PROPERTIES and msg.member == "Get":
            interface, name = msg.body
            if name == "version" and interface in (GLOBAL, REGISTRY):
                version = self.options.get("version", 2) if interface == GLOBAL else 1
                return Message.new_method_return(msg, "v", [Variant("u", version)])
        if msg.interface == PROPERTIES and msg.member == "GetAll":
            return Message.new_method_return(
                msg, "a{sv}", [{"version": Variant("u", self.options.get("version", 2))}]
            )
        if msg.interface == REGISTRY and msg.member == "Register":
            failure = self.options.get("registry_error")
            if failure:
                return Message.new_error(msg, failure, "Mock registry failure")
            app_id, options = msg.body
            assert msg.signature == "sa{sv}"
            assert app_id == self.options.get("expected_app_id", "ai.wisprflow.Flow"), app_id
            assert options == {}
            assert msg.sender not in self.registered, "Register called twice"
            assert not any(s["sender"] == msg.sender for s in self.sessions)
            self.registered[msg.sender] = app_id
            if hook := self.options.get("after_register"):
                self.tasks.append(asyncio.create_task(self.reply_after_register(msg, hook)))
                return True
            return Message.new_method_return(msg)
        if msg.interface == GLOBAL and msg.member == "CreateSession":
            assert msg.signature == "a{sv}", msg.signature
            if not self.options.get("registry_error"):
                assert msg.sender in self.registered, "Register missing on caller connection"
            options = msg.body[0]
            sender = msg.sender[1:].replace(".", "_")
            token = options["session_handle_token"].value
            assert options["session_handle_token"].signature == "s"
            path = f"{PATH}/session/{sender}/{token}"
            self.sessions.append({
                "path": path, "sender": msg.sender, "shortcuts": [],
                "registered": self.registered.get(msg.sender),
            })
            result = {} if self.options.get("missing_session") else {
                "session_handle": Variant("s", path)
            }
            return self.complete_request(msg, options, result)
        if msg.interface == GLOBAL and msg.member == "BindShortcuts":
            assert msg.signature == "oa(sa{sv})sa{sv}", msg.signature
            session, shortcuts, parent, options = msg.body
            record = next(s for s in self.sessions if s["path"] == session)
            assert record["sender"] == msg.sender
            assert parent == ""
            self.bind_counts[session] = self.bind_counts.get(session, 0) + 1
            assert self.bind_counts[session] == 1, "BindShortcuts repeated in session"
            record["shortcuts"] = shortcuts
            assert all(s[1]["description"].signature == "s" for s in shortcuts)
            assert all(s[1]["preferred_trigger"].signature == "s" for s in shortcuts)
            selected = self.options.get("approved_ids")
            bound = [
                [sid, {
                    "description": properties["description"],
                    "trigger_description": Variant(
                        "s", "" if self.options.get("empty_trigger")
                        else self.options.get("trigger", properties["preferred_trigger"].value)
                    ),
                }]
                for sid, properties in shortcuts
                if selected is None or sid in selected
            ]
            result = {} if self.options.get("missing_shortcuts") else {
                "shortcuts": Variant("a(sa{sv})", bound)
            }
            if "early_change" in self.options:
                changed = [[sid, {
                    "description": Variant("s", sid),
                    "trigger_description": Variant("s", trigger),
                }] for sid, trigger in self.options["early_change"]]
                # This reaches the wire before both the BindShortcuts method
                # reply and its Response signal, which still claims approval.
                self.bus.send(Message.new_signal(
                    PATH, GLOBAL, "ShortcutsChanged", "oa(sa{sv})", [session, changed]
                ))
            return self.complete_request(
                msg, options, result, self.options.get("bind_response", 0)
            )
        if msg.interface == GLOBAL and msg.member == "ListShortcuts":
            # Persisted mappings are deliberately returned even before binding.
            # The test still requires one BindShortcuts for each new session.
            return self.complete_request(msg, msg.body[1], {
                "shortcuts": Variant("a(sa{sv})", [["ptt", {
                    "description": Variant("s", "Persisted dictate"),
                    "trigger_description": Variant("s", "Ctrl+Meta+D"),
                }]])
            })
        if msg.interface == SESSION and msg.member == "Close":
            self.closed.append(msg.path)
            return Message.new_method_return(msg)
        if msg.interface == REQUEST and msg.member == "Close":
            self.closed.append(msg.path)
            return Message.new_method_return(msg)
        return Message.new_error(
            msg, "org.freedesktop.DBus.Error.UnknownMethod", "Not in mock interface"
        )

    async def reply_after_register(self, msg, hook):
        try:
            await hook(self)
            await self.bus.send(Message.new_method_return(msg))
        except Exception as error:
            self.errors.append(repr(error))
            await self.bus.send(Message.new_error(
                msg, "org.freedesktop.DBus.Error.Failed", repr(error)
            ))

    def complete_request(self, msg, options, result, response=0):
        sender = msg.sender[1:].replace(".", "_")
        token = options["handle_token"].value
        assert options["handle_token"].signature == "s"
        suffix = "_returned" if self.options.get("different_handle") else ""
        path = f"{PATH}/request/{sender}/{token}{suffix}"
        reply = Message.new_signal(path, REQUEST, "Response", "ua{sv}", [response, result])
        reply.destination = msg.sender
        if alien := self.options.get("spoof_response_bus"):
            forged = Message.new_signal(path, REQUEST, "Response", "ua{sv}", [1, {}])
            forged.destination = msg.sender
            alien.send(forged)

        def deliver_response():
            self.bus.send(reply)
            if msg.member == "BindShortcuts" and "after_response_change" in self.options:
                shortcuts = [[sid, {
                    "description": Variant("s", sid),
                    "trigger_description": Variant("s", trigger),
                }] for sid, trigger in self.options["after_response_change"]]
                self.bus.send(Message.new_signal(
                    PATH, GLOBAL, "ShortcutsChanged", "oa(sa{sv})", [msg.body[0], shortcuts]
                ))

        if self.options.get("pause_bind") and msg.member == "BindShortcuts":
            self.pending.append(reply)
        elif self.options.get("response_after"):
            asyncio.get_running_loop().call_later(0.01, deliver_response)
        else:
            # Queue the signal before the method reply: valid, and easy to lose
            # if a client only subscribes after obtaining its request handle.
            deliver_response()
        return Message.new_method_return(msg, "o", [path])

    async def wait_calls(self, member, count=1):
        async def wait():
            while len([m for m in self.calls if m.member == member]) < count:
                self.changed.clear()
                await self.changed.wait()
        try:
            await asyncio.wait_for(wait(), TIMEOUT)
        except asyncio.TimeoutError as error:
            helper = getattr(self, "helper", None)
            diagnostics = bytes(helper.stderr).decode(errors="replace") if helper else ""
            raise AssertionError(
                f"Expected {count} {member} calls; got {[m.member for m in self.calls]}; "
                f"portal errors {self.errors}; helper stderr: {diagnostics}"
            ) from error
        if self.errors:
            raise AssertionError(self.errors)
        return [m for m in self.calls if m.member == member]

    async def emit(self, member, sid="ptt", timestamp=0, session=None,
                   bus=None, interface=GLOBAL, path=PATH, signature="osta{sv}",
                   body=None):
        sender_bus = bus or self.bus
        if body is None:
            body = [session or self.sessions[-1]["path"], sid, timestamp, {}]
        await sender_bus.send(Message.new_signal(path, interface, member, signature, body))

    async def burst(self, signals):
        pending = []
        for member, sid, timestamp in signals:
            body = [self.sessions[-1]["path"], sid, timestamp, {}]
            pending.append(self.bus.send(
                Message.new_signal(PATH, GLOBAL, member, "osta{sv}", body)
            ))
        await asyncio.gather(*pending)

    async def close_session(self):
        await self.emit("Closed", path=self.sessions[-1]["path"],
                        interface=SESSION, signature="a{sv}", body=[{}])

    async def change(self, ids):
        shortcuts = [[sid, {
            "description": Variant("s", sid),
            "trigger_description": Variant("s", trigger),
        }] for sid, trigger in ids]
        await self.emit("ShortcutsChanged", signature="oa(sa{sv})",
                        body=[self.sessions[-1]["path"], shortcuts])


class Helper:
    """Electron's stdin/fd-3 topology without modifying the parent's fd 3."""

    def __init__(self, address, temporary, extra_env=None, trace=False):
        self.address = address
        self.temporary = temporary
        self.extra_env = extra_env or {}
        self.trace = trace
        self.proc = None
        self.transport = None
        self.messages = []
        self.raw_frames = []
        self.changed = asyncio.Event()
        self.reader_error = None
        self.stderr = bytearray()
        self.sequence = 0

    async def start(self):
        read_fd, write_fd = os.pipe()
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "DBUS_SESSION_BUS_ADDRESS": self.address,
            "XDG_CONFIG_HOME": str(self.temporary / "config"),
            "XDG_DATA_HOME": str(self.temporary / "data"),
            "XDG_CACHE_HOME": str(self.temporary / "cache"),
            "XDG_RUNTIME_DIR": str(self.temporary),
            "RUST_LOG": "info",
            "WISPR_CAPTURE": "portal",
            **self.extra_env,
        }
        wrapper = (
            "import os,sys\n"
            "fd=int(sys.argv[1])\n"
            "os.dup2(fd,3)\n"
            "if fd != 3: os.close(fd)\n"
            "os.execv(sys.argv[2],sys.argv[2:])\n"
        )
        command = [sys.executable, "-c", wrapper, str(write_fd), str(BINARY)]
        if self.trace:
            command = ["strace", "-f", "-e", "trace=open,openat,openat2",
                       "-o", str(self.temporary / "opens.trace"), *command]
        try:
            self.proc = await asyncio.create_subprocess_exec(
                *command, stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
                pass_fds=(write_fd,), env=env,
            )
        except Exception:
            os.close(read_fd)
            raise
        finally:
            os.close(write_fd)
        reader = asyncio.StreamReader()
        self.transport, _ = await asyncio.get_running_loop().connect_read_pipe(
            lambda: asyncio.StreamReaderProtocol(reader),
            os.fdopen(read_fd, "rb", buffering=0),
        )
        self.read_task = asyncio.create_task(self.read_frames(reader))
        self.stderr_task = asyncio.create_task(self.read_stderr())
        self.stdout_task = asyncio.create_task(self.proc.stdout.read())
        return self

    async def read_stderr(self):
        while chunk := await self.proc.stderr.read(4096):
            self.stderr.extend(chunk)
            self.changed.set()
        return bytes(self.stderr)

    async def wait_registered(self):
        # BindShortcuts being called is earlier than consent being consumed.
        # This barrier observes registration only; a subsequent real transport
        # activation and its IPC keys are still required to verify delivery.
        await self.wait_for(lambda: (
            b"GlobalShortcuts registered PTT" in self.stderr
            or b"PTT has no active trigger" in self.stderr
        ))

    async def read_frames(self, reader):
        pending = b""
        try:
            while chunk := await reader.read(4096):
                pending += chunk
                while b"|" in pending:
                    raw, pending = pending.split(b"|", 1)
                    self.raw_frames.append(raw)
                    body = raw.decode().replace("+2", "|").replace("+1", "+")
                    self.messages.append(json.loads(body))
                    self.changed.set()
            if pending:
                raise AssertionError(f"Incomplete fd-3 frame: {pending!r}")
        except Exception as error:
            self.reader_error = error
        finally:
            self.changed.set()

    async def wait_for(self, predicate, timeout=TIMEOUT):
        async def wait():
            while not predicate():
                if self.reader_error:
                    raise self.reader_error
                if self.read_task.done():
                    stderr = await self.stderr_task
                    raise AssertionError(f"Helper fd 3 closed: {stderr.decode()}")
                self.changed.clear()
                await self.changed.wait()
        try:
            await asyncio.wait_for(wait(), timeout)
        except asyncio.TimeoutError as error:
            raise AssertionError(
                f"Helper response timed out; keys={self.keys()}; "
                f"stderr={bytes(self.stderr).decode(errors='replace')}"
            ) from error

    async def request(self, command, payload=True, uuid=None, timeout=TIMEOUT):
        self.sequence += 1
        uuid = uuid or f"request-{self.sequence}"
        self.proc.stdin.write(frame({
            "HelperAPIRequest": {command: payload, "uuid": uuid}
        }))
        await self.proc.stdin.drain()
        def result():
            return next((m["HelperAPIResponse"] for m in self.messages
                         if m.get("HelperAPIResponse", {}).get("uuid") == uuid), None)
        await self.wait_for(lambda: result() is not None, timeout)
        return result()

    def keys(self):
        return [m["HelperAPIRequest"]["KeypressEvent"]["payload"]
                for m in self.messages
                if "KeypressEvent" in m.get("HelperAPIRequest", {})]

    async def wait_keys(self, count):
        await self.wait_for(lambda: len(self.keys()) >= count)
        return self.keys()

    async def stale(self, keys=(162, 91, 27)):
        response = await self.request("CheckStaleKeys", {
            "payload": {"keycodes": list(keys)}
        })
        return response["StaleKeysResponse"]["payload"]["staleKeys"]

    async def stop(self):
        if self.proc is None:
            return
        if self.proc.returncode is None:
            self.proc.stdin.close()
            try:
                await asyncio.wait_for(self.proc.wait(), 2)
            except asyncio.TimeoutError:
                self.proc.kill()
                await self.proc.wait()
        await asyncio.gather(self.read_task, self.stderr_task, self.stdout_task)
        if self.transport:
            self.transport.close()


class PortalIntegration(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        if not BINARY.is_file():
            self.fail(f"Build helper first, or set WISPR_TEST_HELPER: {BINARY}")
        if not shutil.which("dbus-daemon"):
            self.fail("dbus-daemon is required for real protocol tests")
        self.directory = tempfile.TemporaryDirectory(prefix="wispr-portal-")
        self.addCleanup(self.directory.cleanup)
        self.temporary = Path(self.directory.name)
        self.settings_path = self.temporary / "config" / "Wispr Flow" / "config.json"
        self.write_shortcuts({"162+91": "ptt", "27": "dismiss"})
        config = self.temporary / "dbus.conf"
        transport = os.environ.get("WISPR_TEST_BUS_TRANSPORT", "unix")
        self.assertIn(transport, ("unix", "tcp"))
        listener = (
            '<listen>tcp:host=127.0.0.1,bind=127.0.0.1,port=0,family=ipv4</listen>'
            '<auth>ANONYMOUS</auth><allow_anonymous/>'
            if transport == "tcp" else
            f'<listen>unix:path={self.temporary}/bus</listen><auth>EXTERNAL</auth>'
        )
        config.write_text(
            "<busconfig><type>session</type>"
            + listener + '<policy context="default">'
            '<allow user="*"/><allow own="*"/>'
            '<allow send_destination="*"/><allow receive_sender="*"/>'
            "</policy></busconfig>"
        )
        self.daemon = await asyncio.create_subprocess_exec(
            "dbus-daemon", "--nofork", "--print-address=1",
            "--config-file=" + str(config),
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        )
        self.address = (await asyncio.wait_for(
            self.daemon.stdout.readline(), TIMEOUT
        )).decode().strip()
        if not self.address:
            stderr = await self.daemon.stderr.read()
            await self.daemon.wait()
            self.fail("Private D-Bus daemon could not start: " + stderr.decode())
        self.assertTrue(self.address.startswith(transport + ":"), self.address)
        self.portals = []
        self.helpers = []
        self.extra_buses = []

    async def asyncTearDown(self):
        failures = []
        for helper in self.helpers:
            await helper.stop()
            if helper.reader_error:
                failures.append(str(helper.reader_error))
            if await helper.stdout_task:
                failures.append("IPC leaked to stdout")
        for bus in self.extra_buses:
            await disconnect_bus(bus)
        for portal in self.portals:
            await portal.stop()
            failures.extend(portal.errors)
        if self.daemon.returncode is None:
            self.daemon.terminate()
        await self.daemon.communicate()
        self.directory.cleanup()
        self.assertEqual(failures, [])

    async def launch(self, *, helper_env=None, trace=False, **portal_options):
        portal = await MockPortal(self.address, **portal_options).start()
        self.portals.append(portal)
        helper = await Helper(self.address, self.temporary, helper_env, trace).start()
        portal.helper = helper
        self.helpers.append(helper)
        await helper.request("IsReady")
        return portal, helper

    def write_shortcuts(self, shortcuts, preserve_mtime=False):
        previous = self.settings_path.stat() if preserve_mtime else None
        self.settings_path.parent.mkdir(parents=True, exist_ok=True)
        contents = json.dumps({"prefs": {"user": {"shortcuts": shortcuts}}})
        replacement = self.settings_path.with_suffix(".new")
        replacement.write_text(contents)
        replacement.replace(self.settings_path)
        if previous:
            os.utime(self.settings_path, ns=(previous.st_atime_ns, previous.st_mtime_ns))
        return contents

    async def assert_injection_blocked(self, helper):
        for command, payload in (
            ("PasteText", {"text": "must not paste", "htmlText": ""}),
            ("SimulateKeyPress", {"keycode": 86, "flags": ["Control"]}),
        ):
            response = await helper.request(command, {"payload": payload})
            self.assertIn("HelperAPIError", response)
            reason = response["HelperAPIError"]["payload"]["description"]
            # The stub itself returns errors: only this guard-specific result
            # proves the command was rejected before reaching that backend.
            self.assertIn("portal", reason.lower())
            self.assertNotIn("stub", reason.lower())
        self.assertTrue((await helper.request("IsReady"))["ACK"])

    async def activate(self, portal, helper):
        await portal.wait_calls("BindShortcuts")
        await helper.wait_registered()
        await portal.emit("Activated")
        await helper.wait_keys(2)

    def assert_key_pairs(self, helper, pairs):
        self.assertEqual(
            [(k["key"], k["eventType"]) for k in helper.keys()], pairs
        )
        self.assertEqual([k["index"] for k in helper.keys()],
                         list(range(1, len(pairs) + 1)))
        self.assertTrue(all(k["inputType"] == "keyboard" for k in helper.keys()))

    async def test_default_press_release_and_zero_kde_timestamps(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        self.assertEqual(await helper.stale(), [27])
        await portal.emit("Activated")
        await portal.emit("Deactivated")
        await portal.emit("Deactivated")
        await helper.wait_keys(4)
        self.assertEqual(await helper.stale(), [162, 91, 27])
        self.assert_key_pairs(helper, [
            (162, "key_event_press"), (91, "key_event_press"),
            (91, "key_event_release"), (162, "key_event_release"),
        ])
        self.assertTrue(all(b"\n" in raw for raw in helper.raw_frames))

    async def test_cancel_pulses_then_releases_ptt_and_ignores_held_repeats(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        bindings = portal.sessions[-1]["shortcuts"]
        self.assertEqual({sid: props["preferred_trigger"].value
                          for sid, props in bindings},
                         {"ptt": "F8", "cancel": "F9"})
        await portal.emit("Activated", sid="cancel")
        await helper.wait_keys(6)
        self.assertEqual(await helper.stale(), [162, 91, 27])
        await portal.emit("Activated", sid="cancel")
        await portal.emit("Activated")
        await portal.emit("Deactivated", sid="cancel")
        await portal.emit("Deactivated")
        await asyncio.sleep(0.05)
        self.assert_key_pairs(helper, [
            (162, "key_event_press"), (91, "key_event_press"),
            (27, "key_event_press"), (27, "key_event_release"),
            (91, "key_event_release"), (162, "key_event_release"),
        ])

    async def test_kde_superseding_shortcut_release_then_dismiss_sequence(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        # kglobalacceld releases its last shortcut before activating another.
        # This asserts delivered IPC only, not proprietary-app cancellation.
        await portal.emit("Deactivated")
        await portal.emit("Activated", sid="cancel")
        await portal.emit("Deactivated", sid="cancel")
        await helper.wait_keys(6)
        self.assert_key_pairs(helper, [
            (162, "key_event_press"), (91, "key_event_press"),
            (91, "key_event_release"), (162, "key_event_release"),
            (27, "key_event_press"), (27, "key_event_release"),
        ])

    async def test_registry_identity_and_one_bind_each_new_session(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        first = portal.sessions[-1]
        await helper.stop()
        another = await Helper(self.address, self.temporary).start()
        self.helpers.append(another)
        await another.request("IsReady")
        await portal.wait_calls("BindShortcuts", 2)
        await another.wait_registered()
        await portal.emit("Activated")
        await another.wait_keys(2)
        second = portal.sessions[-1]
        self.assertNotEqual(first["path"], second["path"])
        self.assertNotEqual(first["sender"], second["sender"])
        self.assertEqual([s["registered"] for s in portal.sessions],
                         ["ai.wisprflow.Flow", "ai.wisprflow.Flow"])
        self.assertEqual(list(portal.bind_counts.values()), [1, 1])
        self.assertEqual(len([m for m in portal.calls if m.member == "Register"]), 2)

    async def test_immediate_response_with_nonpredicted_request_handle(self):
        portal, helper = await self.launch(different_handle=True)
        await self.activate(portal, helper)
        await portal.emit("Deactivated")
        await helper.wait_keys(4)

    async def test_appimage_identity_override_is_registered_before_portal_use(self):
        app_id = "ai.wisprflow.Flow.Test"
        portal, helper = await self.launch(
            helper_env={"WISPR_PORTAL_APP_ID": app_id}, expected_app_id=app_id
        )
        await self.activate(portal, helper)
        self.assertEqual(portal.sessions[-1]["registered"], app_id)
        members = [call.member for call in portal.calls]
        self.assertLess(members.index("Register"), members.index("CreateSession"))
        await portal.emit("Deactivated")
        await helper.wait_keys(4)

    async def test_invalid_app_id_stops_before_any_portal_operation(self):
        portal, helper = await self.launch(helper_env={
            "WISPR_PORTAL_APP_ID": "../wispr-flow"
        })
        await asyncio.sleep(0.1)
        self.assertEqual(portal.calls, [])
        self.assertEqual(helper.keys(), [])
        self.assertEqual(await helper.stale(), [162, 91, 27])

    async def test_restart_during_registration_never_uses_unregistered_new_owner(self):
        replaced = asyncio.Event()
        replacement = MockPortal(self.address)

        async def replace_owner(portal):
            await portal.bus.release_name(SERVICE)
            await replacement.start()
            self.portals.append(replacement)
            replaced.set()

        portal, helper = await self.launch(after_register=replace_owner)
        await asyncio.wait_for(replaced.wait(), TIMEOUT)
        await asyncio.sleep(0.1)
        self.assertEqual(replacement.calls, [])
        self.assertEqual(replacement.sessions, [])
        self.assertEqual(helper.keys(), [])
        self.assertEqual(await helper.stale(), [162, 91, 27])
        self.assertTrue((await helper.request("IsReady"))["ACK"])
        self.assertEqual(len(portal.registered), 1)

    async def test_response_after_method_reply(self):
        portal, helper = await self.launch(response_after=True)
        await self.activate(portal, helper)
        await portal.emit("Deactivated")
        await helper.wait_keys(4)

    async def test_pending_consent_does_not_block_isready(self):
        portal, helper = await self.launch(pause_bind=True)
        await portal.wait_calls("BindShortcuts")
        started = time.monotonic()
        response = await helper.request("IsReady", uuid="keep+alive|test", timeout=1.0)
        self.assertTrue(response["ACK"])
        self.assertLess(time.monotonic() - started, 1.0)
        self.assertEqual(await helper.stale(), [162, 91, 27])
        self.assertEqual(helper.keys(), [])
        self.assertEqual(len(portal.pending), 1)
        await self.assert_injection_blocked(helper)

    async def test_session_closed_during_consent_stops_waiting(self):
        portal, helper = await self.launch(pause_bind=True)
        await portal.wait_calls("BindShortcuts")
        await portal.close_session()
        await portal.wait_calls("Close")
        await portal.bus.send(portal.pending[0])
        await portal.emit("Activated")
        await asyncio.sleep(0.05)
        self.assertEqual(helper.keys(), [])
        self.assertEqual(await helper.stale(), [162, 91, 27])

    async def test_permission_cancel_is_not_retried(self):
        portal, helper = await self.launch(bind_response=1)
        await portal.wait_calls("BindShortcuts")
        await asyncio.sleep(0.1)
        await portal.emit("Activated")
        await asyncio.sleep(0.1)
        self.assertEqual(helper.keys(), [])
        self.assertEqual(await helper.stale(), [162, 91, 27])
        self.assertEqual(list(portal.bind_counts.values()), [1])
        await portal.wait_calls("Close")
        await self.assert_injection_blocked(helper)

    async def test_wrong_sender_session_id_interface_and_path_are_ignored(self):
        portal, helper = await self.launch()
        await portal.wait_calls("BindShortcuts")
        alien = await connect_bus(self.address)
        self.extra_buses.append(alien)
        await helper.wait_registered()
        invalid = [
            {"bus": alien}, {"session": PATH + "/session/other"},
            {"sid": "not-approved"}, {"interface": "org.example.Impostor"},
            {"path": PATH + "/wrong"},
        ]
        for options in invalid:
            # Complete pulses ensure accidental acceptance cannot hide behind
            # deduplication of the following known-good activation.
            await portal.emit("Activated", **options)
            await portal.emit("Deactivated", **options)
        await alien.send(Message.new_signal(
            "/org/freedesktop/DBus", "org.freedesktop.DBus", "NameOwnerChanged", "sss",
            [SERVICE, portal.bus.unique_name, alien.unique_name],
        ))
        await portal.emit("Closed", bus=alien, path=portal.sessions[-1]["path"],
                          interface=SESSION, signature="a{sv}", body=[{}])
        await portal.emit("ShortcutsChanged", bus=alien, signature="oa(sa{sv})",
                          body=[portal.sessions[-1]["path"], []])
        await alien.call(Message(destination="org.freedesktop.DBus",
                                 path="/org/freedesktop/DBus",
                                 interface="org.freedesktop.DBus", member="GetId"))
        await portal.emit("Activated")
        await helper.wait_keys(2)
        self.assert_key_pairs(helper, [(162, "key_event_press"), (91, "key_event_press")])

    async def test_session_closed_releases_and_rejects_late_activation(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        await portal.close_session()
        await helper.wait_keys(6)
        await portal.emit("Activated")
        await asyncio.sleep(0.1)
        self.assert_key_pairs(helper, FAULT_KEYS)
        self.assertEqual(await helper.stale(), [162, 91, 27])
        await self.assert_injection_blocked(helper)

    async def test_portal_owner_loss_releases_without_automatic_reconnect(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        await portal.bus.release_name(SERVICE)
        await helper.wait_keys(6)
        await portal.bus.request_name(SERVICE)
        await portal.emit("Activated")
        await asyncio.sleep(0.1)
        self.assert_key_pairs(helper, FAULT_KEYS)
        self.assertEqual(await helper.stale(), [162, 91, 27])
        self.assertEqual(len(portal.sessions), 1)
        await self.assert_injection_blocked(helper)

    async def test_bus_disconnect_releases_held_keys(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        self.daemon.terminate()
        await self.daemon.wait()
        await helper.wait_keys(6)
        self.assert_key_pairs(helper, FAULT_KEYS)
        self.assertEqual(await helper.stale(), [162, 91, 27])
        await self.assert_injection_blocked(helper)

    async def test_kde_backend_loss_cancels_with_frontend_still_running(self):
        backend = await connect_bus(self.address)
        self.extra_buses.append(backend)
        await backend.request_name(KDE_SERVICE)
        portal, helper = await self.launch(helper_env={"XDG_CURRENT_DESKTOP": "KDE"})
        await self.activate(portal, helper)
        await backend.release_name(KDE_SERVICE)
        await helper.wait_keys(6)
        await backend.request_name(KDE_SERVICE)
        await portal.emit("Deactivated")
        await portal.emit("Activated")
        await asyncio.sleep(0.1)
        self.assert_key_pairs(helper, FAULT_KEYS)
        self.assertEqual(len(portal.sessions), 1)
        self.assertIn(b"KDE portal backend owner changed", helper.stderr)
        await self.assert_injection_blocked(helper)

    async def test_forged_kde_backend_loss_is_ignored(self):
        backend = await connect_bus(self.address)
        self.extra_buses.append(backend)
        await backend.request_name(KDE_SERVICE)
        portal, helper = await self.launch(helper_env={"XDG_CURRENT_DESKTOP": "KDE"})
        await self.activate(portal, helper)
        await backend.send(Message.new_signal(
            "/org/freedesktop/DBus", "org.freedesktop.DBus", "NameOwnerChanged", "sss",
            [KDE_SERVICE, backend.unique_name, ""],
        ))
        await portal.emit("Deactivated")
        await helper.wait_keys(4)
        self.assert_key_pairs(helper, [
            (162, "key_event_press"), (91, "key_event_press"),
            (91, "key_event_release"), (162, "key_event_release"),
        ])

    async def test_kde_backend_loss_during_consent_blocks_capture(self):
        backend = await connect_bus(self.address)
        self.extra_buses.append(backend)
        await backend.request_name(KDE_SERVICE)
        portal, helper = await self.launch(
            helper_env={"XDG_CURRENT_DESKTOP": "KDE"}, pause_bind=True,
        )
        await portal.wait_calls("BindShortcuts")
        await backend.release_name(KDE_SERVICE)
        await helper.wait_for(lambda: b"KDE portal backend owner changed" in helper.stderr)
        self.assertEqual(helper.keys(), [])
        await self.assert_injection_blocked(helper)

    async def test_fault_after_ptt_release_still_cancels_processing_or_handsfree(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        await portal.emit("Deactivated")
        await helper.wait_keys(4)
        await portal.close_session()
        await helper.wait_keys(6)
        await portal.wait_calls("Close")
        await asyncio.sleep(0.05)
        self.assert_key_pairs(helper, [
            (162, "key_event_press"), (91, "key_event_press"),
            (91, "key_event_release"), (162, "key_event_release"),
            (27, "key_event_press"), (27, "key_event_release"),
        ])
        self.assertEqual(await helper.stale(), [162, 91, 27])
        await self.assert_injection_blocked(helper)

    async def test_fault_uses_custom_dismiss_even_if_its_physical_binding_denied(self):
        self.write_shortcuts({"162+91": "ptt", "164+27": "dismiss"})
        portal, helper = await self.launch(approved_ids=["ptt"])
        await self.activate(portal, helper)
        await portal.emit("Activated", sid="cancel")
        await portal.close_session()
        await helper.wait_keys(8)
        self.assert_key_pairs(helper, [
            (162, "key_event_press"), (91, "key_event_press"),
            (164, "key_event_press"), (27, "key_event_press"),
            (27, "key_event_release"), (164, "key_event_release"),
            (91, "key_event_release"), (162, "key_event_release"),
        ])

    async def test_missing_portal_stays_ready_without_capture(self):
        helper = await Helper(self.address, self.temporary).start()
        self.helpers.append(helper)
        self.assertTrue((await helper.request("IsReady"))["ACK"])
        await asyncio.sleep(0.1)
        self.assertEqual(await helper.stale(), [162, 91, 27])
        self.assertEqual(helper.keys(), [])
        await self.assert_injection_blocked(helper)

    async def test_partial_binding_only_approves_returned_ids(self):
        portal, helper = await self.launch(approved_ids=["ptt"])
        await self.activate(portal, helper)
        await portal.emit("Activated", sid="cancel")
        await portal.emit("Deactivated")
        await helper.wait_keys(4)
        self.assertEqual([k["key"] for k in helper.keys()], [162, 91, 91, 162])

    async def test_empty_binding_does_not_report_working_capture(self):
        portal, helper = await self.launch(empty_trigger=True)
        await portal.wait_calls("BindShortcuts")
        await asyncio.sleep(0.1)
        await portal.emit("Activated")
        await asyncio.sleep(0.05)
        self.assertEqual(helper.keys(), [])
        self.assertEqual(await helper.stale(), [162, 91, 27])

    async def test_revoked_binding_requires_restart_even_after_later_rebind(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        await portal.change([("ptt", "")])
        await helper.wait_keys(6)
        await portal.emit("Activated")
        await asyncio.sleep(0.05)
        self.assert_key_pairs(helper, FAULT_KEYS)
        self.assertEqual(await helper.stale(), [162, 91, 27])
        await portal.change([("ptt", "Ctrl+Alt+Space")])
        await portal.emit("Activated")
        await asyncio.sleep(0.05)
        self.assert_key_pairs(helper, FAULT_KEYS)
        self.assertEqual(await helper.stale(), [162, 91, 27])
        self.assertEqual(list(portal.bind_counts.values()), [1])
        await self.assert_injection_blocked(helper)

    async def test_malformed_active_signal_fails_closed(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        await portal.emit("Activated", signature="s", body=["malformed"])
        await helper.wait_keys(6)
        self.assert_key_pairs(helper, FAULT_KEYS)
        self.assertTrue((await helper.request("IsReady"))["ACK"])
        self.assertEqual(await helper.stale(), [162, 91, 27])

    async def test_invalid_registry_identity_fails_closed(self):
        portal, helper = await self.launch(
            registry_error="org.freedesktop.portal.Error.Failed"
        )
        await portal.wait_calls("Register")
        await asyncio.sleep(0.1)
        self.assertEqual(portal.sessions, [])
        self.assertEqual(await helper.stale(), [162, 91, 27])

    async def test_older_registry_unknown_method_retains_portal_only(self):
        portal, helper = await self.launch(
            registry_error="org.freedesktop.DBus.Error.UnknownMethod"
        )
        await self.activate(portal, helper)
        await portal.emit("Deactivated")
        await helper.wait_keys(4)

    async def test_older_registry_unknown_interface_retains_portal_only(self):
        portal, helper = await self.launch(
            registry_error="org.freedesktop.DBus.Error.UnknownInterface"
        )
        await self.activate(portal, helper)
        await portal.emit("Deactivated")
        await helper.wait_keys(4)

    async def test_missing_session_result_fails_closed(self):
        portal, helper = await self.launch(missing_session=True)
        await portal.wait_calls("CreateSession")
        await asyncio.sleep(0.05)
        self.assertEqual(portal.bind_counts, {})
        self.assertEqual(await helper.stale(), [162, 91, 27])

    async def test_missing_binding_result_closes_session(self):
        portal, helper = await self.launch(missing_shortcuts=True)
        await portal.wait_calls("Close")
        self.assertEqual(helper.keys(), [])
        self.assertEqual(await helper.stale(), [162, 91, 27])

    async def test_custom_configuration_emits_its_windows_virtual_keys(self):
        self.write_shortcuts({"164+83": "ptt", "27": "dismiss"})
        portal, helper = await self.launch()
        await portal.wait_calls("BindShortcuts")
        await helper.wait_registered()
        await portal.emit("Activated")
        await helper.wait_keys(2)
        self.assertEqual(await helper.stale((164, 83)), [])
        await portal.emit("Deactivated")
        await helper.wait_keys(4)
        self.assert_key_pairs(helper, [
            (164, "key_event_press"), (83, "key_event_press"),
            (83, "key_event_release"), (164, "key_event_release"),
        ])

    async def test_burst_preserves_release_before_activation_with_nonmonotonic_times(self):
        portal, helper = await self.launch()
        await portal.wait_calls("BindShortcuts")
        await helper.wait_registered()
        await portal.burst([
            ("Deactivated", "ptt", 99),
            ("Activated", "ptt", 0), ("Deactivated", "ptt", 0),
            ("Activated", "ptt", 5), ("Deactivated", "ptt", 3),
            ("Activated", "ptt", 0), ("Deactivated", "ptt", 0),
            ("Activated", "cancel", 0), ("Deactivated", "cancel", 0),
        ])
        await helper.wait_keys(14)
        chord = [
            (162, "key_event_press"), (91, "key_event_press"),
            (91, "key_event_release"), (162, "key_event_release"),
        ]
        self.assert_key_pairs(helper, chord * 3 + [
            (27, "key_event_press"), (27, "key_event_release"),
        ])

    async def test_request_response_from_another_bus_sender_is_ignored(self):
        alien = await connect_bus(self.address)
        self.extra_buses.append(alien)
        portal, helper = await self.launch(
            spoof_response_bus=alien, response_after=True, different_handle=True
        )
        await self.activate(portal, helper)
        await portal.emit("Deactivated")
        await helper.wait_keys(4)
        self.assertEqual(await helper.stale(), [162, 91, 27])

    async def test_permission_pending_gate_opens_after_approval(self):
        portal, helper = await self.launch(pause_bind=True)
        await portal.wait_calls("BindShortcuts")
        await self.assert_injection_blocked(helper)
        await portal.bus.send(portal.pending[0])
        await helper.wait_registered()
        await portal.emit("Activated")
        await helper.wait_keys(2)
        response = await helper.request("SimulateKeyPress", {
            "payload": {"keycode": 86, "flags": ["Control"]}
        })
        # This environment has no display. Reaching its stub is the positive
        # control that the portal gate opened; it is not successful insertion.
        reason = response["HelperAPIError"]["payload"]["description"]
        self.assertIn("stub", reason)
        await portal.close_session()
        await helper.wait_keys(6)
        await self.assert_injection_blocked(helper)

    async def test_none_trigger_is_inactive_despite_successful_registration(self):
        portal, helper = await self.launch(trigger="  NoNe  ")
        await portal.wait_calls("BindShortcuts")
        await helper.wait_registered()
        await portal.emit("Activated")
        await asyncio.sleep(0.03)
        self.assertEqual(helper.keys(), [])
        await self.assert_injection_blocked(helper)

    async def test_only_cancel_approved_cannot_enable_ptt_or_insertion(self):
        portal, helper = await self.launch(approved_ids=["cancel"])
        await portal.wait_calls("BindShortcuts")
        await helper.wait_registered()
        await portal.emit("Activated")
        await asyncio.sleep(0.03)
        self.assertEqual(helper.keys(), [])
        await self.assert_injection_blocked(helper)
        # Initial lack of a trigger is not revocation. KDE can approve one
        # without requiring a new session or a second logical configuration.
        await portal.change([("ptt", "F10"), ("cancel", "F9")])
        await portal.emit("Activated")
        await helper.wait_keys(2)
        self.assert_key_pairs(helper, [
            (162, "key_event_press"), (91, "key_event_press"),
        ])

    async def test_idle_physical_binding_change_keeps_logical_app_configuration(self):
        before = self.settings_path.read_bytes()
        portal, helper = await self.launch()
        await portal.wait_calls("BindShortcuts")
        await helper.wait_registered()
        await portal.change([("ptt", "Ctrl+Alt+Space"), ("cancel", "F10")])
        await portal.emit("Activated")
        await helper.wait_keys(2)
        self.assert_key_pairs(helper, [
            (162, "key_event_press"), (91, "key_event_press"),
        ])
        self.assertEqual(self.settings_path.read_bytes(), before)
        self.assertEqual(list(portal.bind_counts.values()), [1])

    async def test_physical_binding_change_while_held_cancels_and_latches(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        await portal.change([("ptt", "Ctrl+Alt+Space"), ("cancel", "F10")])
        await helper.wait_keys(6)
        self.assert_key_pairs(helper, FAULT_KEYS)
        await portal.emit("Deactivated")
        await portal.emit("Activated")
        await asyncio.sleep(0.03)
        self.assert_key_pairs(helper, FAULT_KEYS)
        await self.assert_injection_blocked(helper)

    async def test_physical_binding_change_during_processing_cancels_and_latches(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        await portal.emit("Deactivated")
        await helper.wait_keys(4)
        await helper.request("DictationStop")
        await portal.change([("ptt", "Ctrl+Alt+Space"), ("cancel", "F10")])
        await helper.wait_keys(6)
        self.assert_key_pairs(helper, [
            (162, "key_event_press"), (91, "key_event_press"),
            (91, "key_event_release"), (162, "key_event_release"),
            (27, "key_event_press"), (27, "key_event_release"),
        ])
        await self.assert_injection_blocked(helper)

    async def test_fresh_install_waits_for_valid_app_settings_without_defaults(self):
        self.settings_path.unlink()
        portal, helper = await self.launch()
        await asyncio.sleep(0.05)
        self.assertEqual(portal.bind_counts, {})
        self.assertFalse(self.settings_path.exists())
        await self.assert_injection_blocked(helper)
        self.write_shortcuts({"63": "ptt", "27": "dismiss"})
        await helper.request("UpdateShortcuts", {"payload": {}})
        await asyncio.sleep(0.05)
        self.assertEqual(portal.bind_counts, {})
        self.write_shortcuts({"162+91": "ptt", "27": "dismiss"})
        await helper.request("UpdateShortcuts", {"payload": {}})
        await self.activate(portal, helper)
        self.assertEqual(list(portal.bind_counts.values()), [1])

    async def test_helper_shutdown_drains_cancellation_and_ack_before_exit(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        uuid = "shutdown+correlation|test"
        response = await helper.request("HelperAppShutdown", uuid=uuid)
        self.assertTrue(response["ACK"])
        await asyncio.wait_for(helper.proc.wait(), TIMEOUT)
        await asyncio.wait_for(helper.read_task, TIMEOUT)
        self.assertEqual(helper.proc.returncode, 0)
        self.assertIsNone(helper.reader_error)
        self.assert_key_pairs(helper, FAULT_KEYS)
        key_positions = [
            index for index, message in enumerate(helper.messages)
            if "KeypressEvent" in message.get("HelperAPIRequest", {})
        ]
        ack_positions = [
            index for index, message in enumerate(helper.messages)
            if message.get("HelperAPIResponse", {}).get("uuid") == uuid
        ]
        self.assertEqual(len(ack_positions), 1)
        self.assertLess(key_positions[-1], ack_positions[0])

    async def test_idle_app_shortcut_update_syncs_without_rebinding_kde(self):
        portal, helper = await self.launch()
        await portal.wait_calls("BindShortcuts")
        await helper.wait_registered()
        saved = self.write_shortcuts({"163+92": "ptt", "27": "dismiss"})
        await helper.request("UpdateShortcuts", {"payload": {}})
        await asyncio.sleep(0.2)
        await portal.emit("Activated")
        await helper.wait_keys(2)
        await portal.emit("Deactivated")
        await helper.wait_keys(4)
        self.assert_key_pairs(helper, [
            (163, "key_event_press"), (92, "key_event_press"),
            (92, "key_event_release"), (163, "key_event_release"),
        ])
        self.assertEqual(self.settings_path.read_text(), saved)
        self.assertEqual(list(portal.bind_counts.values()), [1])

    async def test_completed_cancel_allows_later_idle_configuration_update(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        await portal.emit("Activated", sid="cancel")
        await helper.wait_keys(6)
        await portal.emit("Deactivated")
        await portal.emit("Deactivated", sid="cancel")
        await asyncio.sleep(0.05)
        self.write_shortcuts({"163+92": "ptt", "27": "dismiss"})
        await helper.request("UpdateShortcuts", {"payload": {}})
        await asyncio.sleep(0.2)
        await portal.emit("Activated")
        await helper.wait_keys(8)
        self.assert_key_pairs(helper, FAULT_KEYS + [
            (163, "key_event_press"), (92, "key_event_press"),
        ])
        self.assertEqual(list(portal.bind_counts.values()), [1])

    async def test_content_poll_detects_atomic_same_mtime_configuration_update(self):
        portal, helper = await self.launch()
        await portal.wait_calls("BindShortcuts")
        await helper.wait_registered()
        mtime = self.settings_path.stat().st_mtime_ns
        self.write_shortcuts({"163+92": "ptt", "27": "dismiss"}, preserve_mtime=True)
        self.assertEqual(self.settings_path.stat().st_mtime_ns, mtime)
        # Deliberately do not send UpdateShortcuts: exercise the content poll.
        await asyncio.sleep(3.2)
        await portal.emit("Activated")
        await helper.wait_keys(2)
        self.assert_key_pairs(helper, [
            (163, "key_event_press"), (92, "key_event_press"),
        ])
        self.assertEqual(list(portal.bind_counts.values()), [1])

    async def test_app_shortcut_update_during_recording_cancels_with_new_dismiss(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        self.write_shortcuts({"163+92": "ptt", "164+27": "dismiss"})
        await helper.request("UpdateShortcuts", {"payload": {}})
        await helper.wait_keys(8)
        # The app has already changed its bindings. Clear the old chord before
        # pulsing the new Dismiss, so another combined binding cannot intercept
        # cancellation. The insertion gate closes before either event sequence.
        self.assert_key_pairs(helper, [
            (162, "key_event_press"), (91, "key_event_press"),
            (91, "key_event_release"), (162, "key_event_release"),
            (164, "key_event_press"), (27, "key_event_press"),
            (27, "key_event_release"), (164, "key_event_release"),
        ])
        await self.assert_injection_blocked(helper)
        self.assertEqual(list(portal.bind_counts.values()), [1])

    async def test_removed_app_ptt_uses_new_dismiss_and_requires_restart(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        self.write_shortcuts({"164+27": "dismiss"})
        await helper.request("UpdateShortcuts", {"payload": {}})
        await helper.wait_keys(8)
        self.assert_key_pairs(helper, [
            (162, "key_event_press"), (91, "key_event_press"),
            (91, "key_event_release"), (162, "key_event_release"),
            (164, "key_event_press"), (27, "key_event_press"),
            (27, "key_event_release"), (164, "key_event_release"),
        ])
        self.write_shortcuts({"162+91": "ptt", "164+27": "dismiss"})
        await helper.request("UpdateShortcuts", {"payload": {}})
        await portal.emit("Activated")
        await asyncio.sleep(0.05)
        self.assertEqual(len(helper.keys()), 8)
        await self.assert_injection_blocked(helper)

    async def test_binding_response_supersedes_earlier_changed_signal(self):
        portal, helper = await self.launch(early_change=[])
        await self.activate(portal, helper)
        self.assert_key_pairs(helper, [
            (162, "key_event_press"), (91, "key_event_press"),
        ])
        self.assertEqual(list(portal.bind_counts.values()), [1])

    async def test_changed_signal_after_response_overrides_registration(self):
        portal, helper = await self.launch(after_response_change=[])
        await portal.wait_calls("BindShortcuts")
        await helper.wait_registered()
        await portal.emit("Activated")
        await asyncio.sleep(0.05)
        self.assertEqual(helper.keys(), [])
        await self.assert_injection_blocked(helper)
        self.assertEqual(list(portal.bind_counts.values()), [1])

    async def test_app_config_changed_during_consent_uses_current_logical_chord(self):
        portal, helper = await self.launch(pause_bind=True)
        await portal.wait_calls("BindShortcuts")
        self.write_shortcuts({"163+92": "ptt", "164+27": "dismiss"})
        await helper.request("UpdateShortcuts", {"payload": {}})
        await portal.bus.send(portal.pending[0])
        await helper.wait_registered()
        await portal.emit("Activated")
        await helper.wait_keys(2)
        self.assert_key_pairs(helper, [
            (163, "key_event_press"), (92, "key_event_press"),
        ])
        self.assertEqual(list(portal.bind_counts.values()), [1])

    async def test_invalid_current_cancel_releases_without_guessing_old_dismiss(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        self.write_shortcuts({"162+91": "ptt", "not-a-key": "dismiss"})
        await helper.request("UpdateShortcuts", {"payload": {}})
        await helper.wait_keys(4)
        await asyncio.sleep(0.03)
        self.assert_key_pairs(helper, [
            (162, "key_event_press"), (91, "key_event_press"),
            (91, "key_event_release"), (162, "key_event_release"),
        ])
        await self.assert_injection_blocked(helper)

    async def test_unreadable_current_settings_release_without_guessing_dismiss(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        self.settings_path.unlink()
        await helper.request("UpdateShortcuts", {"payload": {}})
        await helper.wait_keys(4)
        await asyncio.sleep(0.03)
        self.assert_key_pairs(helper, [
            (162, "key_event_press"), (91, "key_event_press"),
            (91, "key_event_release"), (162, "key_event_release"),
        ])
        await self.assert_injection_blocked(helper)

    async def test_canonical_none_overrides_legacy_evdev(self):
        portal, helper = await self.launch(helper_env={
            "WISPR_CAPTURE": "none", "WISPR_KEY_CAPTURE": "evdev",
        })
        await asyncio.sleep(0.05)
        self.assertEqual(portal.calls, [])
        self.assertEqual(helper.keys(), [])
        response = await helper.request("SimulateKeyPress", {
            "payload": {"keycode": 86, "flags": []}
        })
        self.assertIn("stub", response["HelperAPIError"]["payload"]["description"])

    async def test_default_helper_has_no_open_physical_event_descriptors(self):
        portal, helper = await self.launch()
        await self.activate(portal, helper)
        descriptor_dir = Path(f"/proc/{helper.proc.pid}/fd")
        if not descriptor_dir.is_dir() and helper.proc.returncode is None:
            self.skipTest("child /proc descriptors unavailable; no FD audit performed")
        descriptors = []
        for fd in descriptor_dir.iterdir():
            with contextlib.suppress(FileNotFoundError):
                descriptors.append(os.readlink(fd))
        self.assertTrue(descriptors, "Must inspect actual descriptors")
        self.assertFalse(any(p.startswith("/dev/input") for p in descriptors), descriptors)

    async def test_default_capture_makes_no_libc_input_open_attempts(self):
        compiler = shutil.which("cc")
        if not compiler:
            self.skipTest("C compiler unavailable; libc open audit not run")
        library = self.temporary / "input-open-audit.so"
        build = await asyncio.create_subprocess_exec(
            compiler, "-shared", "-fPIC", "-Wall", "-Wextra", "-Werror",
            "-o", str(library),
            str(Path(__file__).with_name("input_open_audit.c")), "-ldl",
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await build.communicate()
        self.assertEqual(build.returncode, 0, stderr.decode())
        marker = b"WISPR_TEST_INPUT_AUDIT_ACTIVE"
        denied = b"WISPR_TEST_INPUT_OPEN_DENIED"
        # Prove the interposer runs and rejects access before using its absence
        # of events as evidence. No real physical device is opened by control.
        control = await asyncio.create_subprocess_exec(
            sys.executable, "-c", "import os; os.listdir('/dev/input')",
            env={"LD_PRELOAD": str(library)},
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await control.communicate()
        self.assertNotEqual(control.returncode, 0)
        self.assertIn(marker, stderr)
        self.assertIn(denied + b" /dev/input", stderr)
        self.assertIn(b"PermissionError", stderr)
        portal, helper = await self.launch(helper_env={"LD_PRELOAD": str(library)})
        await self.activate(portal, helper)
        await portal.emit("Deactivated")
        await helper.wait_keys(4)
        await helper.stop()
        stderr = await helper.stderr_task
        # Both the fd-3 remapping wrapper and the executed helper load it.
        self.assertEqual(stderr.count(marker), 2, stderr.decode())
        self.assertNotIn(denied, stderr, stderr.decode())
        # Regression control: the explicit legacy implementation must trigger
        # this same audit, proving it detects a raw-input fallback in the helper.
        legacy = await Helper(self.address, self.temporary, {
            "LD_PRELOAD": str(library), "WISPR_CAPTURE": "evdev",
        }).start()
        self.helpers.append(legacy)
        await legacy.request("IsReady")
        await legacy.stop()
        self.assertIn(denied + b" /dev/input", await legacy.stderr_task)

    async def test_default_helper_never_opens_physical_event_devices(self):
        if not shutil.which("strace"):
            self.skipTest("strace unavailable; no open-syscall audit performed")
        probe = await asyncio.create_subprocess_exec(
            "strace", "-e", "trace=none", "--", "/usr/bin/true",
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        )
        _, stderr = await probe.communicate()
        if probe.returncode != 0 and b"Operation not permitted" in stderr:
            self.skipTest("ptrace denied by environment; no syscall audit performed")
        self.assertEqual(probe.returncode, 0, stderr.decode())
        portal, helper = await self.launch(trace=True)
        await self.activate(portal, helper)
        await portal.emit("Deactivated")
        await helper.wait_keys(4)
        await helper.stop()
        trace = (self.temporary / "opens.trace").read_text()
        self.assertIn("openat(", trace, "Trace must contain actual open syscalls")
        self.assertNotIn("/dev/input", trace, trace)


if __name__ == "__main__":
    unittest.main(verbosity=2)
