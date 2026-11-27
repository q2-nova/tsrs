#!/usr/bin/env python3
"""Start tsrsd offline, check what it reports and stop it.

The daemon runs with the command line q2nd passes, a fresh state directory
and a control URL that refuses connections, so it needs no network. The
smoke checks the LocalAPI status document, the offline health sentence, the
CLI shim's status, one request through the HTTP proxy to a local origin, the
key file and the daemon log, then stops the daemon with SIGTERM.

Pass --qemu and --sysroot to run a Q2 build under qemu-mipsel; without them
the binary runs on the host. See README.md.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import tomllib

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / 'tools'))
import cores  # noqa: E402
QEMU_CPU = "P5600"
WAITING = "Waiting for the Tailscale control server"
# The health sentences the daemon reports without a control server (daemon.rs).
OFFLINE_HEALTH = {WAITING, "No network connection"}
START_LIMIT = 60
# The daemon reports WAITING once control has not answered for 20 s, on its 10 s tick.
WAITING_LIMIT = 60
STOP_LIMIT = 10
ORIGIN_BODY = b"tsrsd smoke\n"
# Log messages of local faults and restarts, which an offline start never
# meets. Control errors are not local faults.
FORBIDDEN_LOG = {"restart", "restart_failed", "panic", "keyfile_error", "socket_error", "proxy_error"}


class Failure(Exception):
    pass


def cargo_version(manifest=ROOT / "Cargo.toml"):
    return tomllib.loads(manifest.read_text())["package"]["version"]


def emulator_prefix(qemu, sysroot):
    return [str(qemu), "-cpu", QEMU_CPU, "-L", str(sysroot)] if qemu else []


def daemon_command(prefix, tsrsd, work, proxy_port):
    """q2nd's tailscaled command line, with the paths under `work`."""
    proxy = f"127.0.0.1:{proxy_port}"
    statedir = work / "state/tailscale"
    return [*prefix, str(tsrsd), "--tun=userspace-networking", f"--outbound-http-proxy-listen={proxy}",
            f"--socks5-server={proxy}", f"--socket={work / 'localapi.sock'}", f"--statedir={statedir}",
            f"--state={statedir / 'tailscaled.state'}"]


def status_problems(doc, version, waiting):
    """What differs in a status document from a daemon that has never reached
    control; `waiting` requires the health sentence it reports after a while."""
    problems = []
    expected = {"Version": f"tsrsd {version}", "Backend": "tailscale-rs", "BackendState": "Starting",
                "AuthURL": "", "TailscaleIPs": []}
    for key, value in expected.items():
        if doc.get(key) != value:
            problems.append(f"{key} is {doc.get(key)!r}, expected {value!r}")
    health = doc.get("Health")
    if not isinstance(health, list):
        problems.append(f"Health is {health!r}, expected a list")
    else:
        problems += [f"unexpected health {h!r}" for h in health if h not in OFFLINE_HEALTH]
        if waiting and WAITING not in health:
            problems.append(f"Health lacks {WAITING!r}")
    return problems


def log_problems(text, version):
    """What the daemon log of one offline start lacks or should not contain."""
    # A line is "tsrsd gN SECONDSs MESSAGE" (log.rs).
    messages = [line.split(" ", 3)[3].split() for line in text.splitlines()
                if line.startswith("tsrsd g") and line.count(" ") >= 3]
    # A start with fresh state.
    required = (["start", f"version={version}"], ["keyfile", "created"], ["listening"])
    problems = [f"log lacks {' '.join(words)!r}" for words in required
                if not any(m[:len(words)] == words for m in messages)]
    problems += [f"log has {' '.join(m)!r}" for m in messages if m and m[0] in FORBIDDEN_LOG]
    return problems


def localapi(path, request_path="/localapi/v0/status"):
    """One LocalAPI request as q2nd makes it: (status code, body)."""
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(10)
        s.connect(str(path))
        s.sendall(f"GET {request_path} HTTP/1.0\r\nHost: local-tailscaled.sock\r\n\r\n".encode())
        response = b"".join(iter(lambda: s.recv(65536), b""))
    head, _, body = response.partition(b"\r\n\r\n")
    return int(head.split(b" ", 2)[1]), body.decode()


def proxied_get(proxy_port, origin_port):
    """GET a local origin through the proxy: (status line, body)."""
    with socket.create_connection(("127.0.0.1", proxy_port), timeout=30) as s:
        s.sendall(f"GET http://127.0.0.1:{origin_port}/smoke HTTP/1.1\r\nHost: 127.0.0.1:{origin_port}\r\n"
                  "Connection: close\r\n\r\n".encode())
        response = b"".join(iter(lambda: s.recv(65536), b""))
    head, _, body = response.partition(b"\r\n\r\n")
    return head.split(b"\r\n", 1)[0].decode(), body


class Origin(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        code = 200 if self.path == "/smoke" else 404
        self.send_response(code)
        self.send_header("Content-Length", str(len(ORIGIN_BODY)))
        self.end_headers()
        self.wfile.write(ORIGIN_BODY)

    def log_message(self, *args):
        pass


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def until(limit, probe):
    """The first true value `probe` returns within `limit` seconds, else None."""
    deadline = time.monotonic() + limit
    while time.monotonic() < deadline:
        value = probe()
        if value:
            return value
        time.sleep(0.5)
    return None


def run(tsrsd, prefix, work, version):
    sock = work / "localapi.sock"
    if len(os.fsencode(sock)) >= 108:
        raise Failure(f"socket path too long for AF_UNIX: {sock}")
    # Bound and not listening: control connections are refused.
    control = socket.socket()
    control.bind(("127.0.0.1", 0))
    origin = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Origin)
    threading.Thread(target=origin.serve_forever, daemon=True).start()
    proxy_port = free_port()
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
           "TS_CONTROL_URL": f"http://127.0.0.1:{control.getsockname()[1]}"}
    if prefix:
        # A re-execution goes through the emulator too (daemon.rs reexec).
        env["TSRSD_EXEC_PREFIX"] = " ".join(prefix)
    log_path = work / "tsrsd.log"
    with log_path.open("wb") as log:
        daemon = subprocess.Popen(daemon_command(prefix, tsrsd, work, proxy_port), cwd=work, env=env,
                                  stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
    try:
        def status():
            if daemon.poll() is not None:
                raise Failure(f"tsrsd exited with {daemon.returncode}")
            try:
                code, body = localapi(sock)
            except OSError:
                return None
            if code != 200:
                raise Failure(f"LocalAPI status answered {code}")
            return json.loads(body)

        first = until(START_LIMIT, status)
        if first is None:
            raise Failure(f"LocalAPI status did not answer within {START_LIMIT} s")
        if problems := status_problems(first, version, waiting=False):
            raise Failure("; ".join(problems))

        line, body = proxied_get(proxy_port, origin.server_address[1])
        if not line.endswith(" 200 OK") or body != ORIGIN_BODY:
            raise Failure(f"proxied request answered {line!r} with {len(body)} bytes")

        shim = subprocess.run([*prefix, str(tsrsd), f"--socket={sock}", "status"], cwd=work, env=env,
                              capture_output=True, text=True, timeout=30)
        if shim.returncode != 0 or shim.stdout != "Starting\n":
            raise Failure(f"tsrsd status exited {shim.returncode} with {shim.stdout!r}")

        def waiting():
            doc = status()
            return doc if doc and WAITING in doc.get("Health", []) else None

        last = until(WAITING_LIMIT, waiting)
        if last is None:
            raise Failure(f"Health lacked {WAITING!r} after {WAITING_LIMIT} s")
        if problems := status_problems(last, version, waiting=True):
            raise Failure("; ".join(problems))

        daemon.send_signal(signal.SIGTERM)
        try:
            daemon.wait(STOP_LIMIT)
        except subprocess.TimeoutExpired:
            raise Failure(f"tsrsd did not stop within {STOP_LIMIT} s of SIGTERM") from None
        if daemon.returncode != -signal.SIGTERM:
            raise Failure(f"tsrsd stopped with {daemon.returncode}, expected {-signal.SIGTERM}")
    finally:
        if daemon.poll() is None:
            daemon.kill()
            daemon.wait()
        origin.shutdown()
        origin.server_close()
        control.close()

    problems = log_problems(log_path.read_text(errors="replace"), version)
    keys = work / "state/tailscale-rs/keys.json"
    if not keys.is_file():
        problems.append("no key file in the tailscale-rs state directory")
    else:
        for path, mode in ((keys, 0o600), (keys.parent, 0o700)):
            if path.stat().st_mode & 0o777 != mode:
                problems.append(f"{path.name} mode {path.stat().st_mode & 0o777:o}, expected {mode:o}")
    if (work / "state/tailscale").exists():
        problems.append("tsrsd wrote to tailscaled's state directory")
    if problems:
        raise Failure("; ".join(problems))
    return last


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--tsrsd", type=Path, required=True, help="the daemon binary")
    p.add_argument("--qemu", type=Path, help="qemu-mipsel, for a Q2 build")
    p.add_argument("--sysroot", type=Path, help="the SDK sysroot, with --qemu")
    a = p.parse_args()
    if bool(a.qemu) != bool(a.sysroot):
        p.error("--qemu and --sysroot go together")
    prefix = emulator_prefix(a.qemu and a.qemu.resolve(), a.sysroot and a.sysroot.resolve())
    version = cargo_version()
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="tsrsd-smoke-") as tmp, cores.swept(tmp):
        work = Path(tmp)
        try:
            status = run(a.tsrsd.resolve(), prefix, work, version)
        except (Failure, OSError, ValueError, subprocess.SubprocessError) as e:
            log = work / "tsrsd.log"
            if log.exists():
                sys.stderr.write(log.read_text(errors="replace"))
            sys.exit(f"smoke: {e}")
    print(json.dumps(dict(passed=True, emulator=f"qemu-mipsel -cpu {QEMU_CPU}" if prefix else None,
                          seconds=round(time.monotonic() - started, 1), status=status)))


if __name__ == "__main__":
    with cores.watched():
        main()
