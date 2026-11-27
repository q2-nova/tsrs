#!/usr/bin/env python3
"""Profile-guided optimization for Q2 release builds of tsrsd.

  tools/pgo.py profile --out DIR BUILD-INPUTS --qemu-user QEMU

BUILD-INPUTS are build.py q2's `--sdk`, `--toolchain-repo`, `--compiler-work`
and `--rust-link`. `profile` builds an instrumented tsrsd (build.py q2
--profile-generate) and a host tsrsd with the `pgo-peer` feature, then
trains the instrumented daemon under qemu-mipsel on a generated tailnet in a
new network namespace: a pinned Headscale for control and its embedded DERP
server, the host peer, and a generated content server, all on 127.0.0.1,
plus a dummy link (192.0.2.1/24, documentation addresses) that carries the
default route the daemon watches. It merges the counts and writes
DIR/tsrsd.profdata and DIR/receipt.json; `build.py q2 --profile-use
DIR/tsrsd.profdata` then refuses a receipt with another compiler or
feature set.

The q2-toolchain driver (tools/q2_pgo.py) supplies the profile runtime, the
instrumentation flags, the qemu runs, the merge, the coverage report and the
receipt. The workloads follow the player's use of the daemon: sign-in,
streaming with fetch-ahead, a bulk download, idle keepalives, a link drop
and return, DERP-only paths, control loss, name lookups and error replies.
Nothing joins a real tailnet or uses an account.
"""
import argparse
import hashlib
import http.server
import importlib.util
import ipaddress
import json
import os
from pathlib import Path
import random
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

HERE = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HERE / 'tools'))
import cores  # noqa: E402
# Bump when the workloads or their durations change.
TRAINING_VERSION = 1
HEADSCALE = {
    'version': '0.29.7',
    'url': 'https://github.com/juanfont/headscale/releases/download/v0.29.7/headscale_0.29.7_linux_amd64',
    'sha256': 'e827fc9b12d958bc95b081c4401dd63f8e0471d025b45b508535a920532150a8',
}
SYSROOT = 'mipsel-buildroot-linux-gnu/sysroot'
BUILD_INPUTS = ('sdk', 'toolchain-repo', 'compiler-work', 'rust-link')
# Tailscale's address ranges: the shared address space of RFC 6598 (written as a
# number) and the ULA prefix.
TAILNET_V4 = str(ipaddress.IPv4Network((0x64400000, 10)))
TAILNET_V6 = 'fd7a:115c:a1e0::/48'
LINK = {'device': 'q2pgo0', 'address': '192.0.2.1/24', 'gateway': '192.0.2.254'}
CONTROL = '127.0.0.1:8443'
CONTENT = ('127.0.0.1', 18080)
PROXY, PEER_PROXY = '127.0.0.1:1055', '127.0.0.1:1056'
PEER, BASE_DOMAIN = 'pgopeer', 'pgo.invalid'
MIB = 1 << 20
SONG_BYTES, SONGS, BULK_BYTES, COVER_BYTES = 4 * MIB, 6, 24 * MIB, 48 * 1024
HOT_PATHS = {
    'packet filter': r'^ts_(bart_packetfilter|packetfilter|packetfilter_state)::',
    'wireguard encrypt/decrypt': r'^ts_tunnel::',
    'derp': r'^ts_derp::',
    'proxy copy loop': r'^tsrsd::proxy::',
}
MODULE_CRATES = ('tsrsd', 'ts_runtime', 'ts_dataplane', 'ts_derp', 'ts_tunnel', 'ts_control', 'tailscale')


def load(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def driver(toolchain):
    path = Path(toolchain) / 'tools/q2_pgo.py'
    if not path.is_file():
        raise SystemExit(f'{path}: the q2-toolchain checkout has no PGO driver')
    return load(path, 'q2_pgo')


def sha256(path):
    with open(path, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def fetch_headscale(cache):
    cache.mkdir(parents=True, exist_ok=True)
    binary = cache / f'headscale-{HEADSCALE["version"]}'
    if not binary.is_file():
        part = binary.with_suffix('.part')
        with urllib.request.urlopen(HEADSCALE['url'], timeout=120) as response, part.open('wb') as out:
            shutil.copyfileobj(response, out)
        part.rename(binary)
    if sha256(binary) != HEADSCALE['sha256']:
        raise SystemExit(f'{binary}: SHA-256 differs from the pinned Headscale release')
    binary.chmod(0o755)
    return binary


# Generated content: songs, covers and a bulk file, served with byte ranges.

def payload(name, size):
    seed = int.from_bytes(hashlib.sha256(name.encode()).digest()[:8], 'little')
    return random.Random(seed).randbytes(size)


def content():
    files = {f'/song/{i}.mp3': payload(f'song-{i}', SONG_BYTES) for i in range(SONGS)}
    files.update({f'/cover/{i}.jpg': payload(f'cover-{i}', COVER_BYTES) for i in range(SONGS)})
    files['/bulk.bin'] = payload('bulk', BULK_BYTES)
    files['/ping'] = b'ok\n'
    return files


class Content(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    files = {}

    def log_message(self, *_):
        pass

    def do_GET(self):
        body = self.files.get(self.path.split('?')[0])
        if body is None:
            self.send_response(404)
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        start, end = 0, len(body) - 1
        ranged = self.headers.get('Range', '')
        if ranged.startswith('bytes='):
            first, _, last = ranged[6:].partition('-')
            start, end = int(first or 0), int(last) if last else len(body) - 1
        self.send_response(206 if ranged else 200)
        self.send_header('Content-Length', str(end - start + 1))
        self.send_header('ETag', '"' + hashlib.sha256(body[:64]).hexdigest()[:16] + '"')
        if ranged:
            self.send_header('Content-Range', f'bytes {start}-{end}/{len(body)}')
        self.end_headers()
        view = memoryview(body)[start:end + 1]
        for offset in range(0, len(view), 64 * 1024):
            self.wfile.write(view[offset:offset + 64 * 1024])


# Clients of the daemon's proxy, as q2nd uses it: absolute-form GET for http
# URLs and CONNECT tunnels (q2nd's https servers).

def get(url, proxy=PROXY, headers=None, timeout=60, read=64 * 1024):
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({'http': 'http://' + proxy}))
    request = urllib.request.Request(url, headers=headers or {})
    total = 0
    with opener.open(request, timeout=timeout) as response:
        while chunk := response.read(read):
            total += len(chunk)
    return total


def tunnel(host, port, path, proxy=PROXY, timeout=60):
    address, proxy_port = proxy.rsplit(':', 1)
    with socket.create_connection((address, int(proxy_port)), timeout=timeout) as sock:
        sock.sendall(f'CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n'.encode())
        reply = b''
        while b'\r\n\r\n' not in reply:
            chunk = sock.recv(4096)
            if not chunk:
                raise OSError('proxy closed the tunnel')
            reply += chunk
        if not reply.startswith(b'HTTP/1.1 200') and not reply.startswith(b'HTTP/1.0 200'):
            raise OSError('tunnel refused: ' + reply.split(b'\r\n', 1)[0].decode(errors='replace'))
        sock.sendall(f'GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n'.encode())
        response = reply.split(b'\r\n\r\n', 1)[1]
        while b'\r\n\r\n' not in response:
            chunk = sock.recv(4096)
            if not chunk:
                raise OSError('tunnel closed before the response')
            response += chunk
        head, body = response.split(b'\r\n\r\n', 1)
        if not head.startswith(b'HTTP/1.1 2'):
            raise OSError('tunnel response: ' + head.split(b'\r\n', 1)[0].decode(errors='replace'))
        length = next(int(line.split(b':', 1)[1]) for line in head.split(b'\r\n')
                      if line.lower().startswith(b'content-length:'))
        total = len(body)
        while total < length:
            chunk = sock.recv(64 * 1024)
            if not chunk:
                raise OSError('tunnel closed mid-body')
            total += len(chunk)
    return total


def attempt(function, *args, **kwargs):
    """An expected failure: the error class, or 'ok'."""
    try:
        function(*args, **kwargs)
        return 'ok'
    except (OSError, ValueError) as error:
        return type(error).__name__


class Tailnet:
    """The generated tailnet inside the current (new) network namespace."""

    def __init__(self, work, headscale, peer, daemon, training, sysroot):
        self.work, self.headscale_binary, self.peer_binary, self.daemon_binary = work, headscale, peer, daemon
        self.training, self.sysroot = training, sysroot
        self.processes, self.phases, self.daemon, self.restarts = [], [], None, []
        self.current = 'start'
        self.work.mkdir(parents=True)
        cores.watch(self.work)
        # Unix socket paths have a 108-byte limit.
        self.sockets = Path(tempfile.mkdtemp(prefix='q2pgo-', dir='/tmp'))

    def ip(self, *args, check=True):
        return subprocess.run(['ip', *args], check=check, capture_output=True, text=True)

    def network(self):
        self.ip('link', 'set', 'lo', 'up')
        self.ip('link', 'add', LINK['device'], 'type', 'dummy')
        self.link_up()

    def link_up(self):
        self.ip('addr', 'replace', LINK['address'], 'dev', LINK['device'])
        self.ip('link', 'set', LINK['device'], 'up')
        self.ip('route', 'replace', 'default', 'via', LINK['gateway'], 'dev', LINK['device'])

    def certificates(self):
        def openssl(*args):
            subprocess.run(['openssl', *args], cwd=self.work, check=True, capture_output=True)
        openssl('req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes', '-keyout', 'ca.key',
                '-out', 'ca.pem', '-days', '2', '-subj', '/CN=Generated tsrsd PGO CA',
                '-addext', 'basicConstraints=critical,CA:TRUE')
        openssl('req', '-new', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes', '-keyout', 'server.key',
                '-out', 'server.csr', '-subj', '/CN=Generated tsrsd PGO control')
        (self.work / 'extensions').write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n'
                                              'extendedKeyUsage=serverAuth\nsubjectAltName=IP:127.0.0.1\n')
        openssl('x509', '-req', '-in', 'server.csr', '-CA', 'ca.pem', '-CAkey', 'ca.key', '-set_serial', '2',
                '-days', '1', '-extfile', 'extensions', '-out', 'server.pem')
        openssl('x509', '-in', 'ca.pem', '-outform', 'DER', '-out', 'ca.der')

    def start(self, name, argv, env=None):
        log = (self.work / (name + '.log')).open('wb')
        process = subprocess.Popen(list(map(str, argv)), env={**os.environ, **(env or {})}, stdout=log,
                                   stderr=subprocess.STDOUT)
        self.processes.append((name, process))
        return process

    def control(self):
        host, port = CONTROL.split(':')
        config = {
            'server_url': f'https://{CONTROL}', 'listen_addr': CONTROL, 'metrics_listen_addr': '',
            'grpc_listen_addr': '', 'noise': {'private_key_path': str(self.work / 'hs-noise.key')},
            'prefixes': {'v4': TAILNET_V4, 'v6': TAILNET_V6, 'allocation': 'sequential'},
            'derp': {'server': {'enabled': True, 'region_id': 901, 'region_code': 'pgo', 'region_name': 'Generated PGO DERP',
                                'verify_clients': True, 'stun_listen_addr': f'{host}:3478',
                                'private_key_path': str(self.work / 'hs-derp.key'),
                                'automatically_add_embedded_derp_region': True, 'ipv4': host},
                     'urls': [], 'paths': [], 'auto_update_enabled': False},
            'disable_check_updates': True,
            'database': {'type': 'sqlite', 'sqlite': {'path': str(self.work / 'hs.sqlite'), 'write_ahead_log': True}},
            'tls_cert_path': str(self.work / 'server.pem'), 'tls_key_path': str(self.work / 'server.key'),
            'policy': {'mode': 'file', 'path': str(self.work / 'policy.json')},
            'dns': {'magic_dns': True, 'base_domain': BASE_DOMAIN, 'override_local_dns': False, 'nameservers': {'global': []}},
            'logtail': {'enabled': False}, 'unix_socket': str(self.sockets / 'hs.sock'), 'unix_socket_permission': '0770',
            'log': {'level': 'info', 'format': 'json'},
        }
        (self.work / 'hs-config.json').write_text(json.dumps(config, indent=2))
        (self.work / 'policy.json').write_text(json.dumps({'acls': [{'action': 'accept', 'src': ['*'], 'dst': ['*:*']}]}))
        self.start_headscale()
        args = [self.headscale_binary, '--config', self.work / 'hs-config.json']
        user = json.loads(subprocess.check_output([*map(str, args), 'users', 'create', 'generated', '--output', 'json']))
        key = json.loads(subprocess.check_output([*map(str, args), 'preauthkeys', 'create', '--user', str(user['id']),
                                                  '--reusable', '--expiration', '2h', '--output', 'json']))
        return key['key']

    def start_headscale(self):
        (self.sockets / 'hs.sock').unlink(missing_ok=True)
        self.headscale = self.start('headscale', [self.headscale_binary, '--config', self.work / 'hs-config.json', 'serve'])
        wait(lambda: (self.sockets / 'hs.sock').exists(), 30, 'Headscale start')

    def stop_headscale(self):
        self.headscale.terminate()
        self.headscale.wait(timeout=30)

    def node_env(self, key, hostname):
        return {'TS_AUTH_KEY': key, 'TS_CONTROL_URL': f'https://{CONTROL}', 'TS_HOSTNAME': hostname,
                'TSRS_EXTRA_CA_DER': str(self.work / 'ca.der')}

    def status(self, name):
        output = subprocess.run([str(self.peer_binary), 'status', '--json', f'--socket={self.sockets / (name + ".sock")}'],
                                capture_output=True, text=True, timeout=30)
        try:
            return json.loads(output.stdout)
        except ValueError:
            return {}

    def phase(self, name, function, *args):
        start = time.monotonic()
        self.current = name
        detail = function(*args)
        # The next phase starts once the daemon serves the peer again (after a
        # recovery in place, a restart, or q2nd's restart of an exited daemon),
        # so disruptions do not overlap.
        ready = time.monotonic()
        wait(lambda: self.supervise() and attempt(get, f'http://{PEER}/ping', timeout=20) == 'ok', 300,
             f'the daemon after {name}')
        self.phases.append({'phase': name, 'seconds': round(ready - start, 1),
                            'recovery_seconds': round(time.monotonic() - ready, 1), **(detail or {})})
        print(json.dumps(self.phases[-1]), flush=True)

    def start_peer(self):
        self.peer = self.start('peer', [self.peer_binary, f'--socket={self.sockets / "peer.sock"}',
                                        f'--statedir={self.work / "peer-state"}', f'--outbound-http-proxy-listen={PEER_PROXY}'],
                               {**self.node_env(self.key, PEER), 'TSRSD_PGO_PEER_FORWARD': f'80={CONTENT[0]}:{CONTENT[1]}'})

    def start_daemon(self):
        qemu = ' '.join(self.training.user_prefix())
        self.daemon = self.training.user_background(
            'tsrsd', [self.daemon_binary, f'--socket={self.sockets / "q2.sock"}', f'--statedir={self.work / "q2-state"}',
                      f'--outbound-http-proxy-listen={PROXY}'],
            env={**self.node_env(self.key, 'q2'), 'TSRSD_EXEC_PREFIX': qemu}, continuous=True,
            stdout=(self.work / 'tsrsd.log').open('ab'), stderr=subprocess.STDOUT)

    def supervise(self):
        """Restart an exited daemon or peer, as q2nd restarts its daemon. Always true."""
        if self.daemon.process.poll() is not None:
            self.restarts.append({'node': 'q2', 'phase': self.current, 'exit': self.daemon.stop()})
            self.start_daemon()
        if self.peer.poll() is not None:
            self.restarts.append({'node': PEER, 'phase': self.current, 'exit': self.peer.returncode})
            self.start_peer()
        return True

    def nodes(self, key):
        """Sign-in and key exchange: the peer natively, the instrumented daemon under qemu-mipsel."""
        self.key = key
        self.start_peer()
        self.start_daemon()
        for name in ('peer', 'q2'):
            wait(lambda: self.status(name).get('BackendState') == 'Running', 300, name + ' sign-in')
        # The peer's name is known and its forward answers through the tailnet.
        wait(lambda: attempt(get, f'http://{PEER}/ping', timeout=10) == 'ok', 300, 'first tailnet request')
        return {'q2_status': self.status('q2').get('BackendState')}

    def lookups(self):
        """Names the proxy resolves: bare, full, missing and a public-style name."""
        results = {}
        for _ in range(4):
            results['bare'] = attempt(get, f'http://{PEER}/ping')
            results['full'] = attempt(get, f'http://{PEER}.{BASE_DOMAIN}/ping')
            results['missing'] = attempt(get, 'http://nosuchpeer/ping', timeout=30)
            results['other'] = attempt(get, 'http://generated.invalid/ping', timeout=30)
        diag = subprocess.run([str(self.peer_binary), f'--socket={self.sockets / "q2.sock"}', 'diag', f'{PEER}:80'],
                              capture_output=True, text=True, timeout=60)
        results['diag'] = diag.returncode
        return {'results': results}

    def stream(self, tracks=3, settle=2.0):
        """The player's pattern: the playing track streams; after a settle the next
        tracks are fetched ahead whole, with their covers; one resumed download."""
        moved = 0
        for track in range(tracks):
            playing = threading.Thread(target=lambda t=track: get(f'http://{PEER}/song/{t}.mp3',
                                                                  headers={'Range': 'bytes=0-'}, read=16 * 1024))
            playing.start()
            moved += get(f'http://{PEER}/cover/{track}.jpg')
            time.sleep(settle)
            for ahead in range(track + 1, min(track + 3, SONGS)):
                moved += tunnel(PEER, 80, f'/song/{ahead}.mp3') if ahead % 2 else get(f'http://{PEER}/song/{ahead}.mp3')
                moved += get(f'http://{PEER}/cover/{ahead}.jpg')
            playing.join()
            moved += SONG_BYTES
        moved += get(f'http://{PEER}/song/{SONGS - 1}.mp3', headers={'Range': f'bytes={SONG_BYTES // 2}-'})
        return {'bytes': moved}

    def bulk(self):
        return {'bytes': get(f'http://{PEER}/bulk.bin', read=256 * 1024, timeout=600)}

    def idle(self, seconds):
        time.sleep(seconds)
        return {'ping': attempt(get, f'http://{PEER}/ping')}

    def link_drop(self, down_seconds):
        """The radio off and back: no address and no route, then a new link."""
        self.ip('link', 'set', LINK['device'], 'down')
        self.ip('addr', 'flush', 'dev', LINK['device'])
        during = attempt(get, f'http://{PEER}/ping', timeout=down_seconds)
        time.sleep(down_seconds)
        self.link_up()
        wait(lambda: attempt(get, f'http://{PEER}/ping', timeout=20) == 'ok', 180, 'request after the link returned')
        return {'during': during, 'after': attempt(get, f'http://{PEER}/song/1.mp3')}

    def derp_only(self):
        """Direct UDP between the nodes dropped: peer traffic stays on DERP."""
        address = LINK['address'].split('/')[0]
        rules = [['nft', 'add', 'table', 'inet', 'q2pgo'],
                 ['nft', 'add', 'chain', 'inet', 'q2pgo', 'out', '{ type filter hook output priority 0 ; }'],
                 ['nft', 'add', 'rule', 'inet', 'q2pgo', 'out', 'ip', 'daddr', address, 'udp', 'dport', '!=', '3478', 'drop']]
        for rule in rules:
            subprocess.run(rule, check=True, capture_output=True)
        try:
            # A link change drops the paths found so far; discovery cannot find new direct ones.
            self.ip('link', 'set', LINK['device'], 'down')
            time.sleep(3)
            self.link_up()
            wait(lambda: attempt(get, f'http://{PEER}/ping', timeout=20) == 'ok', 180, 'DERP-only request')
            moved = sum(get(f'http://{PEER}/song/{i}.mp3') for i in range(2))
        finally:
            subprocess.run(['nft', 'delete', 'table', 'inet', 'q2pgo'], check=False)
        return {'bytes': moved}

    def control_loss(self, seconds):
        """Control goes away and returns; DERP and the peer path stay."""
        self.stop_headscale()
        during = attempt(get, f'http://{PEER}/ping')
        time.sleep(seconds)
        self.start_headscale()
        time.sleep(10)
        return {'during': during, 'after': attempt(get, f'http://{PEER}/ping')}

    def errors(self):
        """Replies the player handles: a refused tailnet port, a refused and an unreachable host."""
        return {'tailnet_refused': attempt(get, f'http://{PEER}:81/', timeout=30),
                'tailnet_tunnel_refused': attempt(tunnel, PEER, 81, '/', timeout=30),
                'direct_refused': attempt(get, 'http://127.0.0.1:9/', timeout=30),
                'not_found': attempt(get, f'http://{PEER}/missing')}

    def close(self):
        if self.daemon is not None and self.daemon.process.poll() is None:
            self.daemon.stop()
        for _, process in reversed(self.processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    process.kill()
        shutil.rmtree(self.sockets, ignore_errors=True)


def wait(condition, seconds, what):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if condition():
            return
        time.sleep(0.5)
    raise SystemExit(f'timed out waiting for {what}')


def train(args):
    """Runs inside the new network namespace (see profile)."""
    pgo = driver(args.toolchain_repo)
    out = args.dir
    training = pgo.Training(out / 'profiles', args.qemu_user, args.sdk / SYSROOT)
    Content.files = content()
    server = http.server.ThreadingHTTPServer(CONTENT, Content)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    daemon = args.daemon or out / 'instrumented/tsrsd'
    net = Tailnet(out / 'tailnet', args.headscale, args.peer, daemon, training, args.sdk / SYSROOT)
    try:
        net.network()
        net.certificates()
        key = net.control()
        net.phase('sign-in', net.nodes, key)
        net.phase('name lookups', net.lookups)
        net.phase('stream with fetch-ahead', net.stream)
        net.phase('bulk download', net.bulk)
        net.phase('idle keepalives', net.idle, args.idle_seconds)
        net.phase('stream with fetch-ahead', net.stream, 2)
        net.phase('control loss', net.control_loss, 20)
        net.phase('link drop and return', net.link_drop, 8)
        net.phase('DERP only', net.derp_only)
        net.phase('error replies', net.errors)
        net.phase('idle keepalives', net.idle, args.idle_seconds)
    finally:
        net.close()
        server.shutdown()
    pgo.write_json(out / 'training.json', {'runs': training.runs, 'phases': net.phases, 'restarts': net.restarts})


def build_args(args):
    command = []
    for name in BUILD_INPUTS:
        command += ['--' + name, str(getattr(args, name.replace('-', '_')).resolve())]
    return command


def profile(args):
    started = time.monotonic()
    out = args.out.resolve()
    if out.exists():
        raise SystemExit(f'{out} exists')
    out.mkdir(parents=True)
    pgo = driver(args.toolchain_repo)
    headscale = fetch_headscale(HERE / 'build/pgo-downloads')
    build_start = time.monotonic()
    subprocess.run([sys.executable, str(HERE / 'build.py'), 'q2', *build_args(args),
                    '--profile-generate', str(out / 'profiles'), '--out', str(out / 'instrumented')], check=True)
    build_minutes = round((time.monotonic() - build_start) / 60, 1)
    peer_target = HERE / 'build/target-pgo-peer'
    subprocess.run(['cargo', '+nightly-2026-11-12', 'build', '--release', '--offline', '--locked', '--features', 'pgo-peer'],
                   cwd=HERE, env=dict(os.environ, CARGO_TARGET_DIR=str(peer_target), CARGO_BUILD_JOBS='2'), check=True)
    (out / 'peer').mkdir()
    peer = out / 'peer/tsrsd'
    shutil.copy2(peer_target / 'release/tsrsd', peer)
    build = json.loads((out / 'instrumented/result.json').read_text())
    if build['profile'] != 'pgo-instrumented' or build['features'] != {'tsrsd': []}:
        raise SystemExit('the instrumented build must have the release feature set')
    train_start = time.monotonic()
    subprocess.run([*args.netns.split(), sys.executable, '-I', str(Path(__file__).resolve()), 'train', '--dir', str(out),
                    '--headscale', str(headscale), '--peer', str(peer), '--toolchain-repo', str(args.toolchain_repo.resolve()),
                    '--qemu-user', str(args.qemu_user.resolve()), '--sdk', str(args.sdk.resolve()),
                    '--idle-seconds', str(args.idle_seconds)], check=True)
    train_minutes = round((time.monotonic() - train_start) / 60, 1)
    record = json.loads((out / 'training.json').read_text())
    training = pgo.Training(out / 'profiles')
    training.runs = record['runs']
    compiler = args.compiler_work.resolve() / 'install'
    profdata = compiler / pgo.PROFDATA
    merged, raw_count = pgo.merge(profdata, training, out / 'tsrsd.profdata')
    report = pgo.coverage(profdata, merged, HOT_PATHS, MODULE_CRATES)
    pgo.write_json(out / 'coverage.json', report)
    receipt = pgo.make_receipt(
        program='tsrsd', profile=merged, training_version=TRAINING_VERSION,
        compiler=pgo.compiler_identity(compiler), features=build['features'],
        coverage_summary=pgo.summary(report), training=training, started=started,
        source_commit=build['source_commit'], raw_profiles=raw_count, phases=record['phases'],
        restarts=record['restarts'],
        instrumented={'sha256': pgo.sha256(out / 'instrumented/tsrsd'), 'build_minutes': build_minutes,
                      'build_result_sha256': pgo.sha256(out / 'instrumented/result.json')},
        fixture={'headscale': HEADSCALE, 'peer_sha256': pgo.sha256(peer), 'namespace_minutes': train_minutes},
        emulator={'qemu_user_sha256': pgo.sha256(args.qemu_user), 'cpu': pgo.QEMU_CPU})
    pgo.write_json(out / 'receipt.json', receipt)
    shutil.rmtree(out / 'profiles')
    if report['hot_paths_missing']:
        raise SystemExit(f'hot paths without counts: {report["hot_paths_missing"]} (see {out / "coverage.json"})')
    print(json.dumps({k: receipt[k] for k in ('profile_sha256', 'minutes', 'raw_profiles')}, indent=1))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest='action', required=True)
    p = sub.add_parser('profile', help='instrumented build, generated tailnet training, merge and receipt')
    for name in BUILD_INPUTS:
        p.add_argument('--' + name, type=Path, required=True)
    p.add_argument('--qemu-user', type=Path, required=True)
    p.add_argument('--out', type=Path, required=True)
    p.add_argument('--idle-seconds', type=int, default=60)
    p.add_argument('--netns', default='unshare --user --map-root-user --net --',
                   help='command prefix that runs training in a new network namespace')
    t = sub.add_parser('train', help=argparse.SUPPRESS)
    for name in ('dir', 'headscale', 'peer', 'toolchain-repo', 'qemu-user', 'sdk'):
        t.add_argument('--' + name, type=Path, required=True)
    t.add_argument('--idle-seconds', type=int, default=60)
    t.add_argument('--daemon', type=Path, help='run these workloads on another Q2 tsrsd (a PGO build, as a check)')
    args = parser.parse_args()
    profile(args) if args.action == 'profile' else train(args)


if __name__ == '__main__':
    with cores.watched():
        main()
