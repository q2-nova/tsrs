#!/usr/bin/env python3
"""Immutable ELF size receipt; static allocation is not measured process RSS."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def measure(binary, readelf):
    binary, readelf = Path(binary).resolve(), Path(readelf).resolve()
    if binary.read_bytes()[:4] != b'\x7fELF':
        raise ValueError('ELF input required')
    before = sha(binary)
    raw = subprocess.check_output([str(readelf), '--wide', '--sections', str(binary)], text=True)
    pattern = re.compile(r'^\s*\[\s*\d+\]\s+(\S+)\s+(\S+)\s+([0-9a-fA-F]+)\s+([0-9a-fA-F]+)\s+([0-9a-fA-F]+)\s+[0-9a-fA-F]+\s+([A-Za-z]*)\s+\d+\s+\d+\s+\d+\s*$')
    sections = []
    for line in raw.splitlines():
        match = pattern.match(line)
        if match:
            name, kind, address, offset, size, flags = match.groups()
            sections.append(dict(name=name, type=kind, address=int(address, 16),
                                 offset=int(offset, 16), bytes=int(size, 16), flags=flags))
    count = re.search(r'There are (\d+) section headers', raw)
    if (not count or len(sections) != int(count.group(1)) - 1
            or not any(s['name'] == '.text' for s in sections)):
        raise ValueError('unrecognized ELF section output or missing text')
    if sha(binary) != before:
        raise ValueError('binary changed during measurement')
    allocated = [s for s in sections if 'A' in s['flags']]
    return dict(schema=1, domain='build-static', binary=str(binary), sha256=before,
                deployed_bytes=binary.stat().st_size,
                allocated_file_bytes=sum(s['bytes'] for s in allocated if s['type'] != 'NOBITS'),
                allocated_zero_fill_bytes=sum(s['bytes'] for s in allocated if s['type'] == 'NOBITS'),
                symbol_table=any(s['name'] == '.symtab' for s in sections),
                debug_sections=[s['name'] for s in sections if s['name'].startswith(('.debug_', '.zdebug_'))],
                sections=sections, readelf_sha256=sha(readelf),
                readelf_version=subprocess.check_output([str(readelf), '--version'], text=True).splitlines()[0],
                raw_sections=raw, runtime_memory_measured=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--readelf', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    result = measure(args.binary, args.readelf)
    with args.out.open('x') as output:
        json.dump(result, output, indent=2)
        output.write('\n')


if __name__ == '__main__':
    main()
