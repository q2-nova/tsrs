#!/usr/bin/env python3
"""Prepare, test and package the standalone Q2 userspace daemon.

See README.md for host and Q2 build examples and required inputs.
"""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
# tailscale-rs main of 2026-11-20, after v0.7.1.
TSRS_COMMIT = 'e88a60083ed2b9b2b8fd62733fe86ea887e72780'
TSRS_URL = 'https://github.com/tailscale/tailscale-rs.git'
SOURCE = HERE / 'build/tailscale-rs'
PATCH = HERE / 'tailscale-rs.patch'
NAME = 'tsrsd'
RUST_TOOLCHAIN = 'nightly-2026-11-12'
SOURCE_DATE_EPOCH = '1793404800'
# The daemon's own inputs: a change to any invalidates a build record.
INPUTS = [
    'Cargo.toml', 'Cargo.lock', 'build.py', 'tailscale-rs.patch',
    *sorted(str(p.relative_to(HERE)) for directory in ('src', 'tools')
            for p in (HERE / directory).glob('*') if p.suffix in ('.rs', '.py')),
]


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def run(args, **kw):
    print('+', ' '.join(map(str, args)), flush=True)
    subprocess.run([str(a) for a in args], check=True, **kw)


def git(*args):
    return subprocess.check_output(['git', '-C', str(SOURCE), *args])


def prepare(upstream=None):
    """Create a fresh patched checkout and admit its exact diff on every use."""
    if not SOURCE.exists():
        SOURCE.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix='prepare-', dir=SOURCE.parent) as temp:
            stage = Path(temp) / 'upstream'
            run(['git', 'clone', '--quiet', '--no-checkout', upstream or TSRS_URL, stage])
            run(['git', '-C', stage, 'checkout', '--quiet', '--detach', TSRS_COMMIT])
            run(['git', '-C', stage, 'apply', '--index', PATCH])
            stage.rename(SOURCE)
    if git('rev-parse', 'HEAD').decode().strip() != TSRS_COMMIT:
        raise SystemExit(f'{SOURCE}: expected upstream commit {TSRS_COMMIT}')
    if git('diff', '--binary', 'HEAD') != PATCH.read_bytes() or git('ls-files', '--others', '--exclude-standard'):
        raise SystemExit(f'{SOURCE}: working source differs from {PATCH.name}')
    return dict(tailscale_rs_commit=TSRS_COMMIT, patch_sha256=sha(PATCH))


def upstream_tests():
    # These dependency tests have their own dev-dependency graph. Limit the
    # fixture workspace to Rust libraries and leave the upstream checkout intact.
    prepare()
    env = dict(os.environ, CARGO_TARGET_DIR=str(HERE / 'build/target-upstream-tests'), CARGO_BUILD_JOBS='2')
    with tempfile.TemporaryDirectory(prefix='upstream-tests-', dir=HERE / 'build') as temp:
        copy = Path(temp) / 'upstream'
        shutil.copytree(SOURCE, copy, ignore=shutil.ignore_patterns('.git'), symlinks=True)
        manifest = copy / 'Cargo.toml'
        manifest.write_text(''.join(line for line in manifest.read_text().splitlines(keepends=True)
                                    if not line.strip().startswith(('"crates/bindings/', '"tools/'))))
        if subprocess.run(['cargo', 'fetch', '--offline'], cwd=copy, env=env, capture_output=True).returncode:
            run(['cargo', 'fetch'], cwd=copy, env=env)
        for crate in ('ts_runtime', 'ts_derp'):
            run(['cargo', 'test', '--offline', '-p', crate, '--lib'], cwd=copy, env=env)


def inputs():
    return {name: sha(HERE / name) for name in INPUTS}


def host(out):
    record = prepare()
    target = HERE / 'build/target-host'
    env = dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_BUILD_JOBS='2')
    run(['cargo', 'build', '--release', '--offline', '--locked'], cwd=HERE, env=env)
    out.mkdir(parents=True, exist_ok=False)
    binary = out / NAME
    binary.write_bytes((target / 'release' / NAME).read_bytes())
    binary.chmod(0o755)
    record.update(passed=True, target='host', sha256=sha(binary), bytes=binary.stat().st_size, inputs=inputs())
    (out / 'result.json').write_text(json.dumps(record, indent=2) + '\n')
    print(json.dumps(dict(binary=str(binary), sha256=record['sha256'])))


def q2(a):
    out, sdk = a.out, a.sdk
    toolchain_repo, compiler_work, rust_link = a.toolchain_repo, a.compiler_work, a.rust_link
    toolchain_cli = toolchain_repo / 'tools/x2000_toolchain.py'
    target_base = toolchain_repo / 'q2/toolchain/mipsel-q2-linux-gnu.json'
    wrapper = toolchain_repo / 'q2/toolchain/q2-x2000-cc'
    prefix = sdk / 'bin/mipsel-buildroot-linux-gnu-'
    cc = Path(str(prefix) + 'gcc')
    ar = Path(str(prefix) + 'ar')
    readelf = Path(str(prefix) + 'readelf')
    objcopy = Path(str(prefix) + 'objcopy')
    for executable in (cc, ar, readelf, objcopy, rust_link):
        if not executable.is_file() or not os.access(executable, os.X_OK):
            raise SystemExit(f'executable required: {executable}')
    if subprocess.check_output(['git', '-C', HERE, 'status', '--porcelain', '--untracked-files=no']):
        raise SystemExit('the Q2 build requires committed daemon sources')
    commit = subprocess.check_output(['git', '-C', HERE, 'rev-parse', 'HEAD'], text=True).strip()
    record = prepare()
    source_inputs = inputs()
    compiler_env = dict(os.environ, Q2_X2000_WORK=str(compiler_work), Q2_SDK=str(sdk))
    run([sys.executable, toolchain_cli, 'check'], env=compiler_env)
    toolchain_receipt = compiler_work / 'install/x2000-toolchain.json'
    record['x2000'] = json.loads(toolchain_receipt.read_text())
    if record['x2000']['pin']['rust_toolchain'] != RUST_TOOLCHAIN:
        raise SystemExit(f'expected the X2000 build of {RUST_TOOLCHAIN}')
    record['build_dependencies'] = {
        'compiler_receipt_sha256': sha(toolchain_receipt), 'target_base_sha256': sha(target_base),
        'c_wrapper_sha256': sha(wrapper), 'rust_link_sha256': sha(rust_link),
        'sdk_gcc_sha256': sha(cc), 'sdk_ar_sha256': sha(ar),
    }
    cache_inputs = [commit, str(sdk), str(compiler_work), record['build_dependencies'],
                    os.environ.get('Q2_X2000_SCHED', '1'), os.environ.get('Q2_X2000_C', '1')]
    cache_key = hashlib.sha256(json.dumps(cache_inputs, sort_keys=True).encode()).hexdigest()[:16]
    target_dir = HERE / 'build' / f'target-q2-{cache_key}'
    target_dir.mkdir(parents=True, exist_ok=True)
    spec = target_dir / 'mipsel-q2-os-linux-gnu.json'
    run([sys.executable, toolchain_cli, 'spec', target_base, spec], env=compiler_env)
    # Import the same environment producer used by the toolchain's CLI.
    sys.path.insert(0, str(toolchain_repo / 'tools'))
    os.environ['Q2_X2000_WORK'] = str(compiler_work)
    import x2000_toolchain
    env = dict(os.environ, CARGO_TARGET_DIR=str(target_dir),
               Q2_OS_CC=str(cc), CC_mipsel_q2_os_linux_gnu=str(cc),
               AR_mipsel_q2_os_linux_gnu=str(ar),
               CFLAGS_mipsel_q2_os_linux_gnu='-mabi=32 -march=mips32r2 -mfp64 -mnan=2008 -mabs=2008',
               CARGO_TARGET_MIPSEL_Q2_OS_LINUX_GNU_LINKER=str(rust_link),
               SOURCE_DATE_EPOCH=SOURCE_DATE_EPOCH, CARGO_BUILD_JOBS='2',
               RUSTFLAGS=f'--remap-path-prefix={HERE}=/tsrs-source --remap-path-prefix={Path.home()}/.cargo=/cargo')
    env.update(x2000_toolchain.cargo_env(sdk=str(sdk)))
    if 'Q2_X2000_CC_ARCH' in env:
        env.pop('CFLAGS_mipsel_q2_os_linux_gnu')
    run(['cargo', '+' + RUST_TOOLCHAIN, 'build', '--release', '--offline', '--locked',
         '-Zbuild-std=std,panic_abort', '-Zbuild-std-features=panic-unwind,backtrace,backtrace-trace-only',
         '-Zjson-target-spec', '--target', spec], cwd=HERE, env=env)
    if source_inputs != inputs() or prepare() != {key: record[key] for key in ('tailscale_rs_commit', 'patch_sha256')}:
        raise SystemExit('daemon sources changed during the build')
    out.mkdir(parents=True, exist_ok=False)
    run([sys.executable, HERE / 'tools/elf_symbols.py', '--source', target_dir / 'mipsel-q2-os-linux-gnu/release' / NAME,
         '--deployed', out / NAME, '--symbols', out / 'symbols', '--prefix', prefix,
         '--toolchain', RUST_TOOLCHAIN, '--source-commit', commit])
    run([sys.executable, HERE / 'tools/elf_size.py', '--binary', out / NAME, '--readelf', readelf, '--out', out / 'size.json'])
    raw = (out / NAME).read_bytes()
    if raw[:6] != b'\x7fELF\x01\x01' or raw[18:20] != b'\x08\x00':
        raise SystemExit('not a little-endian ELF32 MIPS binary')
    record.update(passed=True, target='q2', toolchain=RUST_TOOLCHAIN, source_commit=commit,
                  q2_sha256=sha(out / NAME), bytes=len(raw), symbols_receipt_sha256=sha(out / 'symbols/receipt.json'),
                  size_sha256=sha(out / 'size.json'), inputs=source_inputs)
    (out / 'result.json').write_text(json.dumps(record, indent=2) + '\n')
    print(json.dumps(dict(binary=str(out / NAME), sha256=record['q2_sha256'], bytes=len(raw))))


def admit(out):
    """A Q2 build output a release may ship: its record passed, its binary and
    receipts are the recorded ones, and its inputs are this tree's."""
    out = Path(out)
    record = json.loads((out / 'result.json').read_text())
    binary = out / NAME
    if (record.get('passed') is not True or record.get('target') != 'q2' or binary.is_symlink() or not binary.is_file()
            or record.get('q2_sha256') != sha(binary) or record.get('symbols_receipt_sha256') != sha(out / 'symbols/receipt.json')
            or record.get('size_sha256') != sha(out / 'size.json')):
        raise ValueError('tsrsd must be the binary of a passing build.py q2 output')
    if record.get('inputs') != inputs() or record.get('tailscale_rs_commit') != TSRS_COMMIT or record.get('patch_sha256') != sha(PATCH):
        raise ValueError("tsrsd build inputs differ from this checkout")
    raw = binary.read_bytes()
    if raw[:6] != b'\x7fELF\x01\x01' or raw[18:20] != b'\x08\x00':
        raise ValueError('tsrsd must be a little-endian ELF32 MIPS binary')
    return dict(sha256=record['q2_sha256'], bytes=record['bytes'], source_commit=record['source_commit'],
                tailscale_rs_commit=TSRS_COMMIT, patch_sha256=record['patch_sha256'], record_sha256=sha(out / 'result.json'))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    actions = p.add_subparsers(dest='action', required=True)
    prep = actions.add_parser('prepare')
    prep.add_argument('--tailscale-source', type=lambda path: Path(path).resolve(),
                      help='local Git repository containing the pinned upstream commit')
    actions.add_parser('fetch')
    actions.add_parser('test')
    actions.add_parser('upstream-test')
    for action in ('host', 'q2', 'admit'):
        command = actions.add_parser(action)
        command.add_argument('--out', type=lambda path: Path(path).resolve(), required=True)
    for name in ('sdk', 'toolchain-repo', 'compiler-work', 'rust-link'):
        command = actions.choices['q2']
        command.add_argument('--' + name, type=lambda path: Path(path).resolve(), required=True)
    a = p.parse_args()
    if a.action == 'prepare':
        print(json.dumps(prepare(a.tailscale_source)))
    elif a.action == 'fetch':
        prepare()
        run(['cargo', 'fetch', '--locked'], cwd=HERE)
    elif a.action == 'test':
        prepare()
        env = dict(os.environ, CARGO_TARGET_DIR=str(HERE / 'build/target-host'), CARGO_BUILD_JOBS='2')
        run(['cargo', 'test', '--workspace', '--locked', '--offline'], cwd=HERE, env=env)
        run(['cargo', 'test', '--offline', '--locked', '-p', 'ts_dataplane', '--lib', 'tsmp_reject'],
            cwd=HERE, env=env)
    elif a.action == 'upstream-test':
        upstream_tests()
    elif a.action == 'admit':
        print(json.dumps(admit(a.out)))
    elif a.out.exists() or a.out.is_symlink():
        p.error('--out requires an unused directory')
    elif a.action == 'host':
        host(a.out)
    else:
        q2(a)


if __name__ == '__main__':
    main()
