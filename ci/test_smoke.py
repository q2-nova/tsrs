"""Controls for the smoke's checks, on generated status documents and logs."""
from pathlib import Path
import socket
import tempfile
import threading
import unittest

import smoke

VERSION = smoke.cargo_version()
LOG = f"""tsrsd g0 0.0s start version={VERSION} pid=7
tsrsd g0 0.0s keyfile created mode_fixed=0
tsrsd g0 0.0s listening
"""


def offline(**changes):
    doc = dict(Version=f"tsrsd {VERSION}", Backend="tailscale-rs", BackendState="Starting", AuthURL="",
               TailscaleIPs=[], Health=[])
    doc.update(changes)
    return doc


class Status(unittest.TestCase):
    def test_offline_documents_pass(self):
        self.assertEqual(smoke.status_problems(offline(), VERSION, waiting=False), [])
        for health in ([smoke.WAITING], ["No network connection", smoke.WAITING]):
            self.assertEqual(smoke.status_problems(offline(Health=health), VERSION, waiting=True), [])

    def test_each_difference_is_reported(self):
        for changes in (dict(Version="tsrsd 0.0.0"), dict(Backend="tailscaled"), dict(BackendState="NeedsLogin"),
                        dict(AuthURL="https://login.example/a/1"), dict(TailscaleIPs=["192.0.2.1"]),
                        dict(Health=["Auth key rejected"]), dict(Health=None)):
            with self.subTest(changes):
                self.assertEqual(len(smoke.status_problems(offline(**changes), VERSION, waiting=False)), 1)

    def test_waiting_requires_the_offline_health(self):
        self.assertEqual(smoke.status_problems(offline(Health=["No network connection"]), VERSION, waiting=True),
                         [f"Health lacks {smoke.WAITING!r}"])


class Log(unittest.TestCase):
    def test_a_fresh_start_passes(self):
        self.assertEqual(smoke.log_problems(LOG + "qemu: a line of the emulator\n", VERSION), [])
        self.assertEqual(smoke.log_problems(LOG + "tsrsd g0 3.0s device_error kind=timeout\n", VERSION), [])

    def test_missing_lines(self):
        self.assertEqual(smoke.log_problems(LOG.replace("created", "existing"), VERSION),
                         ["log lacks 'keyfile created'"])
        self.assertEqual(smoke.log_problems(LOG.replace(VERSION, VERSION + "0"), VERSION),
                         [f"log lacks 'start version={VERSION}'"])

    def test_restarts_and_local_faults(self):
        for line in ("restart reason=control_silent next=g1", "panic at=src/daemon.rs:1", "proxy_error kind=AddrInUse"):
            with self.subTest(line):
                self.assertEqual(smoke.log_problems(f"{LOG}tsrsd g0 1.0s {line}\n", VERSION), [f"log has {line!r}"])


class Commands(unittest.TestCase):
    def test_emulator_and_q2nd_command_line(self):
        prefix = smoke.emulator_prefix(Path("/q/qemu-mipsel"), Path("/sdk/sysroot"))
        self.assertEqual(prefix, ["/q/qemu-mipsel", "-cpu", "P5600", "-L", "/sdk/sysroot"])
        self.assertEqual(smoke.emulator_prefix(None, None), [])
        command = smoke.daemon_command(prefix, Path("/b/tsrsd"), Path("/w"), 1055)
        self.assertEqual(command[:6], [*prefix, "/b/tsrsd"])
        self.assertIn("--statedir=/w/state/tailscale", command)
        self.assertIn("--outbound-http-proxy-listen=127.0.0.1:1055", command)


class LocalAPI(unittest.TestCase):
    def test_request_and_reply(self):
        with tempfile.TemporaryDirectory() as tmp, socket.socket(socket.AF_UNIX) as server:
            path = Path(tmp) / "s"
            server.bind(str(path))
            server.listen(1)
            received = []

            def answer():
                conn, _ = server.accept()
                with conn:
                    received.append(conn.recv(4096))
                    conn.sendall(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")

            thread = threading.Thread(target=answer)
            thread.start()
            self.assertEqual(smoke.localapi(path), (200, "{}"))
            thread.join()
            self.assertTrue(received[0].startswith(b"GET /localapi/v0/status HTTP/1.0\r\n"))


if __name__ == "__main__":
    unittest.main()
