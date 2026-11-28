"""The tsrsd notices configuration on generated cargo-about JSON; cargo-about itself is not run.

tools/third_party_notices.py is the q2-player generator; its own tests are in
that repository. These check this repository's tools/notices configuration.
"""
from pathlib import Path
import sys
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT / 'tools'))
import third_party_notices as notices  # noqa: E402

import build  # noqa: E402

TAILSCALE = (ROOT / 'LICENSES/tailscale-rs.txt').read_text().strip('\n')


def crate(name, license, source=None, manifest=None):
    return {'package': {'name': name, 'version': '0.7.1', 'id': name, 'source': source, 'authors': [],
                        'manifest_path': str(manifest)}, 'license': license}


class Config(unittest.TestCase):
    def test_files_and_binary(self):
        config = notices.load_config()
        self.assertEqual(list(config['binaries']), [build.NAME])
        self.assertEqual(config['first_party'], [build.NAME])
        for root in config['license_roots']:
            for file in root['files']:
                self.assertTrue((ROOT / file).is_file(), file)
        self.assertTrue((ROOT / config['binaries'][build.NAME]['manifest']).is_file())

    def test_tailscale_crates_take_the_checkout_license(self):
        source = build.SOURCE
        data = {'crates': [crate('tsrsd', 'GPL-3.0-or-later', manifest=ROOT / 'Cargo.toml'),
                           crate('tailscale', 'BSD-3-Clause', manifest=source / 'Cargo.toml'),
                           crate('ts_runtime', 'BSD-3-Clause', manifest=source / 'crates/ts_runtime/Cargo.toml'),
                           crate('tokio', 'MIT', source='git+https://github.com/q2-nova/tokio?rev=1',
                                 manifest='/git/tokio/tokio/Cargo.toml')],
                'licenses': [{'id': 'BSD-3-Clause', 'text': TAILSCALE, 'source_path': str(source / 'LICENSE'),
                              'used_by': [{'crate': {'id': 'tailscale'}}]},
                             {'id': 'BSD-3-Clause', 'text': 'standard BSD', 'source_path': None,
                              'used_by': [{'crate': {'id': 'ts_runtime'}}]},
                             {'id': 'MIT', 'text': 'MIT, Tokio Contributors', 'source_path': '/git/tokio/LICENSE',
                              'used_by': [{'crate': {'id': 'tokio'}}]}]}
        with mock.patch.object(notices, 'about', return_value=data), \
                mock.patch.object(notices, 'check_tool', return_value='cargo-about 0.9.3'):
            text, summary = notices.generate(build.NAME)
        self.assertEqual(summary['packages'], 3)
        self.assertEqual(summary['standard_text_only'], [])
        self.assertEqual(text.count('Tailscale Inc'), 1)
        self.assertIn('Used by: tailscale 0.7.1, ts_runtime 0.7.1', text)
        self.assertNotIn('standard BSD', text)
        self.assertNotIn('tsrsd 0.7.1', text)


if __name__ == '__main__':
    unittest.main()
