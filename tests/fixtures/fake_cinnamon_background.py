#!/usr/bin/env python3
"""Fake org.Cinnamon.Background service for the restack() integration test.

Owns the well-known bus name, exposes State (0 -> 1 after a short delay, like
the real daemon painting its first frame) and a no-op Start method, and
records every start (pid + monotonic start index) to the JSON file given as
argv[1] so the test can assert on it after SIGTERM finally lets the process
exit (or after the process is killed and a fresh one is D-Bus-activated).
"""
import ctypes
import json
import os
import sys
import signal

import gi

gi.require_version("GLib", "2.0")
gi.require_version("Gio", "2.0")
from gi.repository import GLib, Gio  # noqa: E402

BUS_NAME = "org.Cinnamon.Background"
OBJECT_PATH = "/org/Cinnamon/Background"
IFACE_XML = """
<node>
  <interface name="org.Cinnamon.Background">
    <method name="Start"/>
    <property name="State" type="u" access="read"/>
  </interface>
</node>
"""

STATE_FILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/fake-cinnamon-bg-state.json"
READY_DELAY_MS = 300


def set_comm_like_real_daemon():
    """So restack()'s /proc/<pid>/comm sanity check accepts this fake — the
    real daemon's actual process name, truncated to the kernel's 15-byte
    comm limit exactly like the genuine binary's would be."""
    try:
        libc = ctypes.CDLL("libc.so.6", use_errno=True)
        name = b"cinnamon-background-daemon"[:15]
        libc.prctl(15, name, 0, 0, 0)  # PR_SET_NAME
    except OSError:
        pass


def record_start():
    try:
        with open(STATE_FILE, "r") as f:
            data = json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        data = {"starts": []}
    data["starts"].append({"pid": os.getpid()})
    with open(STATE_FILE, "w") as f:
        json.dump(data, f)


class FakeBackground:
    def __init__(self, loop):
        self.loop = loop
        self.state = 0
        self.node_info = Gio.DBusNodeInfo.new_for_xml(IFACE_XML)
        self.iface_info = self.node_info.interfaces[0]

    def start_ready_timer(self):
        GLib.timeout_add(READY_DELAY_MS, self._go_ready)

    def _go_ready(self):
        self.state = 1
        return GLib.SOURCE_REMOVE

    def handle_method_call(self, conn, sender, path, iface, method, params, invocation):
        if method == "Start":
            # Documented no-op: calling it is what activates the service.
            invocation.return_value(None)
        else:
            invocation.return_dbus_error(
                "org.freedesktop.DBus.Error.UnknownMethod", "no such method"
            )

    def handle_get_property(self, conn, sender, path, iface, name):
        if name == "State":
            return GLib.Variant("u", self.state)
        return None


def main():
    set_comm_like_real_daemon()
    record_start()
    loop = GLib.MainLoop()
    fake = FakeBackground(loop)

    def on_bus_acquired(conn, name):
        conn.register_object(
            OBJECT_PATH,
            fake.iface_info,
            fake.handle_method_call,
            fake.handle_get_property,
            None,
        )
        fake.start_ready_timer()

    def on_name_lost(conn, name):
        loop.quit()

    owner_id = Gio.bus_own_name(
        Gio.BusType.SESSION,
        BUS_NAME,
        Gio.BusNameOwnerFlags.NONE,
        on_bus_acquired,
        None,
        on_name_lost,
    )

    def on_sigterm(*_args):
        GLib.idle_add(loop.quit)

    GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGTERM, on_sigterm)

    loop.run()
    Gio.bus_unown_name(owner_id)


if __name__ == "__main__":
    main()
