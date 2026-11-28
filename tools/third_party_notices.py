#!/usr/bin/env python3
"""Generate and check the third-party notices of the Rust binaries this repository ships.

  tools/third_party_notices.py generate --binary NAME --out FILE [--features F,..]
      [--no-default-features] [--toolchain TOOLCHAIN] [--std-library DIR --std-features F,..]
  tools/third_party_notices.py check [--toolchain TOOLCHAIN] [--std-library DIR --std-features F,..]

A pinned cargo-about (tools/notices/notices.toml) lists every Cargo package in
the binary's dependency graph for the target in tools/notices/about.toml with
the license texts found in the package; with --std-library, also the Rust
standard library built from that rust-src `library` directory. The notices add
the license files of third-party code that tools/notices/notices.toml names as
bundled into a package. The repository's own packages are left out.

Fails when the installed cargo-about is not the pinned release, a license is
not in the accepted list of about.toml, or a package has no license text.
`generate` writes FILE and prints a JSON summary; `check` generates the
notices of every binary of notices.toml and prints their summaries.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import textwrap
import tomllib

ROOT = Path(__file__).resolve().parents[1]
CONFIG_DIR = ROOT / 'tools/notices'
ABOUT = CONFIG_DIR / 'about.toml'
NOTICES = CONFIG_DIR / 'notices.toml'
WIDTH = 78


def sha(data):
    return hashlib.sha256(data).hexdigest()


def config_identity():
    """The hashes of the inputs that decide a notices file, for a build record."""
    return {str(path.relative_to(ROOT)): sha(path.read_bytes()) for path in (ABOUT, NOTICES, Path(__file__).resolve())}


def load_config():
    return tomllib.loads(NOTICES.read_text())


def cargo(toolchain):
    return ['cargo', *(['+' + toolchain] if toolchain else [])]


def check_tool(config, toolchain=None):
    pin = config['cargo_about']
    install = f'cargo install --locked cargo-about --version {pin} --features cli'
    try:
        version = subprocess.run([*cargo(toolchain), 'about', '--version'], capture_output=True, text=True,
                                 check=True).stdout.split()
    except (OSError, subprocess.CalledProcessError):
        raise SystemExit(f'cargo-about {pin} is required: {install}') from None
    if version != ['cargo-about', pin]:
        raise SystemExit(f'cargo-about {pin} is required, found {" ".join(version)}: {install}')
    return f'cargo-about {pin}'


def about(manifest, *, features=(), no_default_features=False, toolchain=None):
    """cargo-about's JSON for the package at `manifest`; fails on an unaccepted license."""
    command = [*cargo(toolchain), 'about', 'generate', '--frozen', '--fail', '--format', 'json',
               '-c', str(ABOUT), '-m', str(manifest)]
    if no_default_features:
        command.append('--no-default-features')
    if features:
        command += ['--features', ' '.join(features)]
    process = subprocess.run(command, capture_output=True, text=True, env=dict(os.environ, NO_COLOR='1'))
    if process.returncode:
        sys.stderr.write(process.stderr)
        raise SystemExit(f'cargo-about rejected the licenses of {manifest}')
    return json.loads(process.stdout)


def inside(path, directory):
    return Path(path).resolve().is_relative_to(Path(directory).resolve())


def read_text(path):
    text = Path(path).read_text(encoding='utf-8', errors='replace').strip('\n')
    if not text.strip():
        raise SystemExit(f'empty license file: {Path(path).relative_to(ROOT)}')
    return text


def packages(data, first_party=(), license_roots=()):
    """The third-party packages of cargo-about's JSON, each with its distinct license texts.

    A text is (license id, text, file name, standard): standard when the package
    ships no license file and cargo-about gives the SPDX text of the license.
    """
    texts = {}
    for entry in data['licenses']:
        for user in entry['used_by']:
            texts.setdefault(user['crate']['id'], []).append(
                (entry['id'], entry['text'].strip('\n'), Path(entry.get('source_path') or '').name,
                 entry.get('source_path') is None))
    result = []
    for crate in data['crates']:
        package = crate['package']
        if package['name'] in first_party:
            if package['source'] is not None or not inside(package['manifest_path'], ROOT):
                raise SystemExit(f"{package['name']}: first-party packages must be paths in this repository")
            continue
        found = texts.get(package['id'], [])
        if package['source'] is None and all(standard for *_, standard in found):
            for root in license_roots:
                if inside(package['manifest_path'], ROOT / root['path']):
                    found = [(root['license'], read_text(ROOT / file), Path(file).name, False)
                             for file in root['files']]
                    break
        if not found or not all(text.strip() for _, text, *_ in found):
            raise SystemExit(f"{package['name']} {package['version']}: no license text")
        result.append({'label': f"{package['name']} {package['version']}", 'expression': crate['license'],
                       'texts': list(dict.fromkeys(found)), 'authors': package.get('authors') or [],
                       'standard': all(standard for *_, standard in found)})
    return sorted(result, key=lambda item: item['label'])


def bundled(config, names):
    """Bundled non-Cargo code whose package is in `names`."""
    return [{'label': f"{entry['name']} ({entry['source']})", 'expression': entry['license'],
             'texts': [(entry['license'], read_text(ROOT / file), Path(file).name, False) for file in entry['files']],
             'authors': [], 'standard': False}
            for entry in config.get('bundled', []) if entry['package'] in names]


def rule(char='-'):
    return char * WIDTH


def render(binary, tool, sections):
    """The notices text: the component lists, then each distinct license text once."""
    groups = {}
    for _, entries in sections:
        for entry in entries:
            for license_id, text, name, _ in entry['texts']:
                title = f'{license_id} ({name})' if name else license_id
                groups.setdefault(text, {'id': title, 'users': []})['users'].append(entry['label'])
    order = sorted(groups, key=lambda text: (groups[text]['id'], groups[text]['users'][0], text))
    number = {text: index for index, text in enumerate(order, 1)}
    lines = [f'Third-party software notices: {binary}', '',
             *textwrap.wrap(f'{binary} contains the third-party software listed below. The license texts '
                            'follow the lists; [n] refers to license text n. A package marked * ships no '
                            'license file: the standard SPDX text of its license is given, and its authors '
                            f'as its Cargo.toml names them are listed. Generated with {tool}.', WIDTH)]
    sections = [(title, entries) for title, entries in sections if entries]
    for title, entries in sections:
        lines += ['', title, rule()]
        width = max(len(entry['label']) + entry['standard'] for entry in entries)
        for entry in entries:
            label = entry['label'] + ('*' if entry['standard'] else '')
            refs = ' '.join(f'[{number[text]}]' for _, text, *_ in entry['texts'])
            lines.append(f"{label.ljust(width)}  {entry['expression']}  {refs}")
    authored = [entry for _, entries in sections for entry in entries if entry['standard']]
    if authored:
        lines += ['', 'Authors of the packages marked *', rule()]
        for entry in authored:
            lines += textwrap.wrap(f"{entry['label']}: {', '.join(entry['authors']) or 'not named'}", WIDTH,
                                   subsequent_indent='    ', break_on_hyphens=False)
    for text in order:
        users = ', '.join(dict.fromkeys(groups[text]['users']))
        lines += ['', rule('='), f"[{number[text]}] {groups[text]['id']}",
                  *textwrap.wrap(f'Used by: {users}', WIDTH, subsequent_indent='    ', break_on_hyphens=False),
                  rule(), '', text]
    output = '\n'.join(line.rstrip() for line in '\n'.join(lines).split('\n')) + '\n'
    for private in {str(Path.home()), str(ROOT)}:
        if private in output:
            raise SystemExit('notices would contain a local path')
    return output


def generate(binary, *, features=None, no_default_features=None, toolchain=None,
             std_library=None, std_features=()):
    """(notices text, summary) for `binary` of notices.toml."""
    config = load_config()
    tool = check_tool(config, toolchain)
    spec = config['binaries'][binary]
    data = about(ROOT / spec['manifest'],
                 features=spec.get('features', []) if features is None else features,
                 no_default_features=spec.get('no_default_features', False) if no_default_features is None
                 else no_default_features, toolchain=toolchain)
    first_party = set(config.get('first_party', []))
    crates = packages(data, first_party, config.get('license_roots', []))
    names = {crate['package']['name'] for crate in data['crates']}
    std = []
    if std_library is not None:
        std = packages(about(Path(std_library) / 'std/Cargo.toml', features=std_features, toolchain=toolchain))
    extra = bundled(config, names)
    text = render(binary, tool, [('Cargo packages', crates), ('Rust standard library, built from source', std),
                                 ('Bundled source code', extra)])
    summary = {
        'binary': binary, 'tool': tool, 'sha256': sha(text.encode()),
        'packages': len(crates), 'std_packages': len(std), 'bundled': [entry['label'] for entry in extra],
        'licenses': sorted({license_id for entry in crates + std + extra for license_id, *_ in entry['texts']}),
        'standard_text_only': [entry['label'] for entry in crates + std if entry['standard']],
    }
    return text, summary


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    actions = parser.add_subparsers(dest='action', required=True)
    make = actions.add_parser('generate')
    make.add_argument('--binary', required=True)
    make.add_argument('--out', type=Path, required=True)
    make.add_argument('--features', type=lambda value: [f for f in value.split(',') if f])
    make.add_argument('--no-default-features', action='store_true', default=None)
    check = actions.add_parser('check')
    for action in (make, check):
        action.add_argument('--toolchain', help='rustup toolchain for cargo metadata')
        action.add_argument('--std-library', type=Path, help='rust-src library directory of the compiler that builds std')
        action.add_argument('--std-features', type=lambda value: [f for f in value.split(',') if f], default=[])
    args = parser.parse_args()
    if args.action == 'generate':
        if args.out.exists():
            parser.error(f'{args.out} exists')
        text, summary = generate(args.binary, features=args.features, no_default_features=args.no_default_features,
                                 toolchain=args.toolchain, std_library=args.std_library,
                                 std_features=args.std_features)
        with args.out.open('x') as stream:
            stream.write(text)
        print(json.dumps(summary, indent=2))
    else:
        print(json.dumps([generate(binary, toolchain=args.toolchain, std_library=args.std_library,
                                   std_features=args.std_features)[1]
                          for binary in load_config()['binaries']], indent=2))


if __name__ == '__main__':
    main()
