#!/usr/bin/env python3
"""Control loss and a late control server, on the generated tailnet of tools/pgo.py.

  python3 ci/control.py --tsrsd build/host/tsrsd --peer PEER

PEER is a host tsrsd built with the `pgo-peer` feature (it may also be the
daemon under test). The test runs itself again in a new user and network
namespace (`unshare`), as tools/pgo.py trains: the pinned Headscale with its
embedded DERP server and a generated certificate authority, the host peer and
a generated content server, all on 127.0.0.1. Nothing joins a real tailnet.

- control loss: with both nodes signed in, Headscale stops for OUTAGE seconds
  and starts again. Neither daemon may exit, panic or restart, and within
  RECOVERY seconds of control's return the peer answers through the daemon
  and the daemon's control connection is back (a reconnect and new map
  messages).
- late control: a third node with fresh state starts while Headscale refuses
  connections and stays Starting for REFUSED seconds. Within RECOVERY seconds
  of Headscale's start it is Running, in the same process, and reaches the
  peer.

The daemon and the peer sign in first; late control runs before control loss,
and each check reports on its own. A run takes about two minutes.
"""
import argparse
import http.server
import importlib.util
import json
from pathlib import Path
import resource
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parent.parent
_spec = importlib.util.spec_from_file_location('tsrs_pgo', ROOT / 'tools/pgo.py')
pgo = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(pgo)

OUTAGE = 40
REFUSED = 30
RECOVERY = 30
SIGN_IN = 60
LATE, LATE_PROXY = 'late', '127.0.0.1:1057'
# Log messages of a daemon that aborted or replaced itself (log.rs, daemon.rs).
FORBIDDEN_LOG = ('panic', 'restart', 'restart_failed')


class Failure(Exception):
    pass


class Daemon:
    """A host daemon, as tools/pgo.py's Tailnet holds the trained one."""

    def __init__(self, process):
        self.process = process

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        return self.process.returncode


class HostTailnet(pgo.Tailnet):
    """tools/pgo.py's tailnet with the daemon under test run on the host."""

    def start_daemon(self):
        self.daemon = Daemon(self.start('tsrsd', [self.daemon_binary, f'--socket={self.sockets / "q2.sock"}',
                                                  f'--statedir={self.work / "q2-state"}',
                                                  f'--outbound-http-proxy-listen={pgo.PROXY}'],
                                        self.node_env(self.key, 'q2')))

    def start_late(self):
        return self.start(LATE, [self.daemon_binary, f'--socket={self.sockets / (LATE + ".sock")}',
                                 f'--statedir={self.work / "late-state"}', f'--outbound-http-proxy-listen={LATE_PROXY}'],
                          self.node_env(self.key, LATE))

    def diag(self, name):
        output = subprocess.run([str(self.peer_binary), f'--socket={self.sockets / (name + ".sock")}', 'diag',
                                 f'{pgo.PEER}:80'], capture_output=True, text=True, timeout=60)
        try:
            return json.loads(output.stdout)
        except ValueError:
            return {}


def until(limit, probe):
    deadline = time.monotonic() + limit
    while time.monotonic() < deadline:
        if value := probe():
            return value
        time.sleep(0.5)
    return None


def log_problems(name, text):
    messages = [line.split(' ', 3)[3] for line in text.splitlines() if line.startswith('tsrsd g') and line.count(' ') >= 3]
    return [f'{name} log has {m!r}' for m in messages if m.split()[0] in FORBIDDEN_LOG]


def check(net, nodes):
    """Each node's process is the one started and its log shows no abort or restart."""
    problems = []
    for name, process in nodes.items():
        if process.poll() is not None:
            problems.append(f'{name} exited with {process.returncode}')
        log = net.work / f'{name}.log'
        problems += log_problems(name, log.read_text(errors='replace') if log.exists() else '')
    if problems:
        raise Failure('; '.join(problems))


def ping(proxy):
    return pgo.attempt(pgo.get, f'http://{pgo.PEER}/ping', proxy=proxy, timeout=10) == 'ok'


def control_loss(net, nodes):
    before = net.diag('q2')
    net.stop_headscale()
    stopped = time.monotonic()
    while time.monotonic() - stopped < OUTAGE:
        check(net, nodes)
        time.sleep(1)
    net.start_headscale()
    returned = time.monotonic()

    def back():
        check(net, nodes)
        if not ping(pgo.PROXY):
            return None
        after = net.diag('q2')
        reconnected = after.get('control_reconnects', 0) > before.get('control_reconnects', 0)
        heard = after.get('map_messages', 0) != before.get('map_messages', 0)
        return after if reconnected and heard else None

    after = until(RECOVERY, back)
    if after is None:
        raise Failure(f'no request through the daemon with control back within {RECOVERY} s of its return')
    return {'recovery_seconds': round(time.monotonic() - returned, 1),
            'control_reconnects': after['control_reconnects']}


def late_control(net, nodes):
    net.stop_headscale()
    late = {LATE: net.start_late()}
    try:
        started = time.monotonic()
        while time.monotonic() - started < REFUSED:
            check(net, late)
            state = net.status(LATE).get('BackendState')
            if state not in (None, 'Starting'):
                raise Failure(f'{LATE} is {state!r} while control refuses connections')
            time.sleep(1)
        net.start_headscale()
        returned = time.monotonic()

        def running():
            check(net, late)
            return net.status(LATE).get('BackendState') == 'Running'

        if not until(RECOVERY, running):
            raise Failure(f'{LATE} not Running within {RECOVERY} s of control starting')
        signed_in = time.monotonic() - returned
        check(net, nodes)
        if not until(RECOVERY, lambda: ping(LATE_PROXY)):
            raise Failure(f'{LATE} Running but no request through it reached the peer')
        return {'sign_in_seconds': round(signed_in, 1)}
    finally:
        late[LATE].terminate()
        late[LATE].wait(timeout=20)


def inside(args):
    """In the new namespace: the tailnet, both checks, the result as JSON."""
    pgo.Content.files = {'/ping': b'ok\n'}
    server = http.server.ThreadingHTTPServer(pgo.CONTENT, pgo.Content)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    net = HostTailnet(args.work / 'tailnet', args.headscale, args.peer, args.tsrsd, None, None)
    result = {}
    try:
        net.network()
        net.certificates()
        net.key = net.control()
        net.start_peer()
        net.start_daemon()
        nodes = {'peer': net.peer, 'tsrsd': net.daemon.process}
        for name in ('peer', 'q2'):
            if not until(SIGN_IN, lambda: net.status(name).get('BackendState') == 'Running'):
                raise Failure(f'{name} did not sign in within {SIGN_IN} s')
        if not until(SIGN_IN, lambda: ping(pgo.PROXY)):
            raise Failure('no first request through the daemon')
        failures = []
        for name, test in (('late_control', late_control), ('control_loss', control_loss)):
            try:
                result[name] = test(net, nodes)
            except Failure as e:
                failures.append(f'{name}: {e}')
            if net.headscale.poll() is not None:
                net.start_headscale()
        if failures:
            result['failure'] = '; '.join(failures)
    except Failure as e:
        result['failure'] = str(e)
    finally:
        net.close()
        server.shutdown()
    print(json.dumps(result))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('--tsrsd', type=Path, required=True, help='the daemon under test (a host build)')
    p.add_argument('--peer', type=Path, help='a host tsrsd with the pgo-peer feature (default: --tsrsd)')
    p.add_argument('--headscale', type=Path, help='the pinned Headscale binary (default: download to build/)')
    p.add_argument('--netns', default='unshare --user --map-root-user --net --',
                   help='command prefix that runs the test in a new network namespace')
    p.add_argument('--inside', action='store_true', help=argparse.SUPPRESS)
    p.add_argument('--work', type=Path, help=argparse.SUPPRESS)
    args = p.parse_args()
    args.peer = args.peer or args.tsrsd
    if args.inside:
        return inside(args)
    headscale = args.headscale or pgo.fetch_headscale(ROOT / 'build/pgo-downloads')
    if pgo.sha256(headscale) != pgo.HEADSCALE['sha256']:
        sys.exit(f'{headscale}: SHA-256 differs from the pinned Headscale release')
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix='tsrsd-control-') as tmp:
        work = Path(tmp)
        run = subprocess.run([*args.netns.split(), sys.executable, '-I', str(Path(__file__).resolve()), '--inside',
                              '--work', str(work), '--tsrsd', str(args.tsrsd.resolve()), '--peer', str(args.peer.resolve()),
                              '--headscale', str(Path(headscale).resolve())], capture_output=True, text=True)
        lines = run.stdout.strip().splitlines()
        result = json.loads(lines[-1]) if lines and lines[-1].startswith('{') else {'failure': run.stderr[-2000:]}
        if 'failure' in result:
            for name in ('tsrsd', 'peer', LATE):
                log = work / 'tailnet' / f'{name}.log'
                if log.exists():
                    sys.stderr.write(f'--- {name}.log\n' + log.read_text(errors='replace'))
            sys.exit(f'control: {result["failure"]}')
    result['seconds'] = round(time.monotonic() - started, 1)
    print(json.dumps(dict(passed=True, **result)))


if __name__ == '__main__':
    # qemu-user writes a guest core into the working directory on a crash,
    # and the core carries the host environment. No dumps.
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    main()
