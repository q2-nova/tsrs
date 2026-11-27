#!/usr/bin/env python3
"""Freeze local symbols and emit a stripped ELF with a hash-bound debug companion."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import zlib

from elf_size import measure, sha


def command(*args):
    return subprocess.check_output([str(a) for a in args], text=True,
                                   env=dict(os.environ, LC_ALL='C'))


def allocated(binary, report):
    data = Path(binary).read_bytes()
    result = []
    import hashlib
    for section in report['sections']:
        if 'A' not in section['flags']:
            continue
        start, size = section['offset'], section['bytes']
        if section['type'] != 'NOBITS' and start + size > len(data):
            raise ValueError('allocated section exceeds file')
        digest = None if section['type'] == 'NOBITS' else hashlib.sha256(data[start:start + size]).hexdigest()
        result.append((section['name'], section['type'], section['address'], size, section['flags'], digest))
    return result


def gdb_script_entries(payload):
    """The names in a Rust .debug_gdb_scripts section, or None if it holds
    anything but its two entry kinds: 0x01 a script file name, 0x04 an inline
    script (name line, then the script), each NUL-terminated."""
    names, at = [], 0
    while at < len(payload):
        kind, end = payload[at], payload.find(b'\0', at + 1)
        if kind not in (1, 4) or end < 0:
            return None
        body = payload[at + 1:end]
        name = body if kind == 1 else body.split(b'\n', 1)[0]
        if not name or not all(32 <= b < 127 for b in name):
            return None
        names.append(name.decode())
        at = end + 1
    return names or None


def verify(full, deployed, debug, prefix):
    before, after = [measure(p, prefix + 'readelf') for p in (full, deployed)]
    if allocated(full, before) != allocated(deployed, after):
        raise ValueError('stripping changed allocated section identity')
    data = Path(deployed).read_bytes()
    retained_metadata = []
    for section in after['sections']:
        if section['name'] not in after['debug_sections']:
            continue
        payload = data[section['offset']:section['offset'] + section['bytes']]
        # Rust's allocated GDB autoload section is not a symbol/line table:
        # std's marker and crates' #[debugger_visualizer] scripts (smol_str has
        # one). GNU strip preserves it; removing it would violate allocation equality.
        scripts = gdb_script_entries(payload) if section['name'] == '.debug_gdb_scripts' and 'A' in section['flags'] else None
        if scripts is not None:
            retained_metadata.append(dict(name=section['name'], bytes=section['bytes'], entries=scripts))
        else:
            raise ValueError('deployment artifact still contains debug symbols')
    if after['symbol_table']:
        raise ValueError('deployment artifact still contains debug symbols')
    after['retained_debug_metadata'] = retained_metadata
    link = next((s for s in after['sections'] if s['name'] == '.gnu_debuglink'), None)
    if link is None:
        raise ValueError('missing debug link')
    body = data[link['offset']:link['offset'] + link['bytes']]
    name = Path(debug).name.encode()
    crc_at = (len(name) + 1 + 3) & ~3
    byteorder = 'little' if data[5] == 1 else 'big'
    if (body[:len(name) + 1] != name + b'\0' or len(body) != crc_at + 4
            or int.from_bytes(body[crc_at:], byteorder) != zlib.crc32(Path(debug).read_bytes())):
        raise ValueError('debug companion CRC/name mismatch')
    # Prove the retained debug file resolves an actual defined function address.
    symbols = []
    for line in command(prefix + 'nm', '-n', '-C', full).splitlines():
        match = re.match(r'^([0-9a-fA-F]+) [tT] (.+)$', line)
        if match and int(match[1], 16):
            symbols.append((match[1], match[2]))
    symbols.sort(key=lambda pair: not (pair[1] == 'main' or pair[1].endswith('::main')))
    symbols = symbols[:128]
    if not symbols:
        raise ValueError('no defined function addresses')
    lines = command(prefix + 'addr2line', '-f', '-C', '-e', debug, *[a for a, _ in symbols]).splitlines()
    witness = next((dict(address=a, symbol=s, function=lines[2*i], location=lines[2*i+1])
                    for i, (a, s) in enumerate(symbols)
                    if 2*i+1 < len(lines) and re.search(r':([1-9][0-9]*)(?:\s|$)', lines[2*i+1])
                    and not lines[2*i+1].startswith('??')), None)
    if witness is None:
        raise ValueError('debug companion cannot resolve a source line')
    return after, witness


def preserve(source, deployed, symbols, prefix, provenance):
    source, deployed, symbols = map(Path, (source, deployed, symbols))
    if deployed.exists() or deployed.is_symlink() or symbols.exists() or symbols.is_symlink():
        raise ValueError('preserving existing artifact/symbol bundle')
    original = measure(source, prefix + 'readelf')
    if not original['symbol_table'] or '.debug_line' not in original['debug_sections']:
        raise ValueError('build must retain symbols and line tables before stripping')
    symbols.mkdir(parents=True)  # exclusive bundle; partial failures remain inspectable
    full, debug = symbols / (deployed.name + '.unstripped'), symbols / (deployed.name + '.debug')
    with full.open('xb') as output, source.open('rb') as input_file:
        shutil.copyfileobj(input_file, output)
    if sha(full) != original['sha256'] or sha(source) != original['sha256']:
        raise ValueError('compiler output changed while freezing symbols')
    command(prefix + 'objcopy', '--only-keep-debug', full, debug)
    command(prefix + 'strip', '-o', deployed, full)
    command(prefix + 'objcopy', '--add-gnu-debuglink=' + str(debug.resolve()), deployed)
    os.chmod(deployed, 0o755)
    size, witness = verify(full, deployed, debug, prefix)
    notes = command(prefix + 'readelf', '--notes', deployed)
    build_id = re.search(r'Build ID: ([0-9a-fA-F]+)', notes)
    receipt = dict(schema=1, passed=True, provenance=provenance,
                   producer_sha256=sha(Path(__file__)), size_reader_sha256=sha(Path(__file__).with_name('elf_size.py')),
                   artifacts={name: dict(path=str(p.resolve()), sha256=sha(p), bytes=p.stat().st_size)
                              for name, p in [('deployed', deployed), ('unstripped', full), ('debug', debug)]},
                   tools={name: sha(Path(prefix + name)) for name in ('readelf', 'objcopy', 'strip', 'nm', 'addr2line')},
                   build_id=build_id[1] if build_id else None, allocated_sections_unchanged=True,
                   debuglink_verified=True, source_line_witness=witness, size=size, hardware=False)
    with (symbols / 'receipt.json').open('x') as output:
        json.dump(receipt, output, indent=2)
        output.write('\n')
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('source', 'deployed', 'symbols'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--prefix', required=True)
    parser.add_argument('--toolchain', required=True)
    parser.add_argument('--source-commit', required=True)
    args = parser.parse_args()
    provenance = dict(source_commit=args.source_commit, toolchain=args.toolchain,
                      rustc=command('rustc', '+' + args.toolchain, '-Vv'),
                      build_environment={k: os.environ.get(k) for k in
                                         ('RUSTFLAGS', 'SOURCE_DATE_EPOCH', 'CARGO_PROFILE_RELEASE_DEBUG', 'CARGO_PROFILE_RELEASE_STRIP')})
    preserve(args.source, args.deployed, args.symbols, args.prefix, provenance)


if __name__ == '__main__':
    main()
