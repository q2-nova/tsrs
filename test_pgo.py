"""Controls for tools/pgo.py that need no compiler, emulator or network namespace."""
import http.server
import importlib.util
import ipaddress
from pathlib import Path
import threading
import unittest
import urllib.error
import urllib.request

import build

ROOT = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location('tsrs_pgo', ROOT / 'tools/pgo.py')
pgo = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(pgo)


class Fixture(unittest.TestCase):
    def test_headscale_is_pinned(self):
        self.assertRegex(pgo.HEADSCALE['sha256'], r'^[0-9a-f]{64}$')
        self.assertIn(pgo.HEADSCALE['version'], pgo.HEADSCALE['url'])
        self.assertTrue(pgo.HEADSCALE['url'].startswith('https://github.com/juanfont/headscale/releases/download/'))

    def test_addresses(self):
        # The shared address space (RFC 6598): neither private nor global.
        tailnet = ipaddress.ip_network(pgo.TAILNET_V4)
        self.assertEqual(tailnet.prefixlen, 10)
        self.assertFalse(tailnet.network_address.is_private or tailnet.network_address.is_global)
        for value in (pgo.CONTROL, pgo.PROXY, pgo.PEER_PROXY, pgo.CONTENT[0]):
            self.assertTrue(value.startswith('127.0.0.1'))
        self.assertIn(ipaddress.ip_interface(pgo.LINK['address']).ip, ipaddress.ip_network('192.0.2.0/24'))

    def test_payloads_are_deterministic(self):
        self.assertEqual(pgo.payload('song-1', 4096), pgo.payload('song-1', 4096))
        self.assertNotEqual(pgo.payload('song-1', 4096), pgo.payload('song-2', 4096))

    def test_content_server_ranges(self):
        pgo.Content.files = {'/song/0.mp3': pgo.payload('song-0', 300000)}
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), pgo.Content)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.shutdown)
        base = f'http://127.0.0.1:{server.server_address[1]}'
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(base + '/song/0.mp3') as whole:
            self.assertEqual(whole.read(), pgo.Content.files['/song/0.mp3'])
        with opener.open(urllib.request.Request(base + '/song/0.mp3', headers={'Range': 'bytes=1000-'})) as part:
            self.assertEqual(part.status, 206)
            self.assertEqual(part.headers['Content-Range'], 'bytes 1000-299999/300000')
            self.assertEqual(part.read(), pgo.Content.files['/song/0.mp3'][1000:])
        with self.assertRaises(urllib.error.HTTPError):
            opener.open(base + '/missing')

    def test_hot_paths_name_the_daemon_paths(self):
        self.assertEqual(set(pgo.HOT_PATHS), {'packet filter', 'wireguard encrypt/decrypt', 'derp', 'proxy copy loop'})
        self.assertRegex('tsrsd::proxy::copy', pgo.HOT_PATHS['proxy copy loop'])

    def test_release_build_has_no_features(self):
        self.assertEqual(build.Q2_FEATURES, {'tsrsd': []})


if __name__ == '__main__':
    unittest.main()
