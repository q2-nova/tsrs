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
sys.path.insert(0, str(HERE / 'tools'))
import cores  # noqa: E402
# tailscale-rs main of 2026-11-20, after v0.7.1.
TSRS_COMMIT = 'e88a60083ed2b9b2b8fd62733fe86ea887e72780'
TSRS_URL = 'https://github.com/tailscale/tailscale-rs.git'
SOURCE = HERE / 'build/tailscale-rs'
PATCH = HERE / 'tailscale-rs.patch'
NAME = 'tsrsd'
RUST_TOOLCHAIN = 'nightly-2026-11-12'
SOURCE_DATE_EPOCH = '1793404800'
# Cargo features of a Q2 build; a profile's receipt must name the same set.
Q2_FEATURES = {NAME: []}
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


def pgo_driver(toolchain_repo):
    path = toolchain_repo / 'tools/q2_pgo.py'
    if not path.is_file():
        raise SystemExit(f'the q2-toolchain checkout has no PGO driver: {path}')
    import importlib.util
    spec = importlib.util.spec_from_file_location('q2_pgo', path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def q2_environment(a):
    """The checked inputs, build record start and Cargo environment of a Q2 build."""
    sdk = a.sdk
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
    if getattr(a, 'profile_use', None) and getattr(a, 'profile_generate', None):
        raise SystemExit('--profile-use and --profile-generate exclude each other')
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
    flags = [f'--remap-path-prefix={HERE}=/tsrs-source', f'--remap-path-prefix={Path.home()}/.cargo=/cargo']
    record['profile'] = 'release'
    record['features'] = Q2_FEATURES
    record['pgo'] = None
    if getattr(a, 'profile_use', None) or getattr(a, 'profile_generate', None):
        driver = pgo_driver(toolchain_repo)
        compiler = driver.compiler_identity(compiler_work / 'install')
    if getattr(a, 'profile_use', None):
        receipt_path = a.profile_use.parent / 'receipt.json'
        try:
            receipt = json.loads(receipt_path.read_text())
            driver.check_receipt(receipt, a.profile_use, compiler=compiler,
                                 feature_set=driver.feature_set_sha256(Q2_FEATURES))
        except (OSError, ValueError) as error:
            raise SystemExit(f'{a.profile_use}: {error}')
        flags += driver.use_flags(a.profile_use)
        record['pgo'] = {'profile_sha256': sha(a.profile_use), 'receipt_sha256': sha(receipt_path), 'receipt': receipt}
    if getattr(a, 'profile_generate', None):
        runtime, _ = driver.runtime(compiler_work / 'install')
        # Continuous counters: the daemon is stopped, or replaces itself, rather than exiting.
        flags += driver.instrument_flags(a.profile_generate, runtime, continuous=True)
        record['profile'] = 'pgo-instrumented'
    cache_inputs = [commit, str(sdk), str(compiler_work), record['build_dependencies'],
                    os.environ.get('Q2_X2000_SCHED', '1'), os.environ.get('Q2_X2000_C', '1'), flags]
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
               CARGO_ENCODED_RUSTFLAGS='\x1f'.join(map(str, flags)))
    env.pop('RUSTFLAGS', None)
    env.update(x2000_toolchain.cargo_env(sdk=str(sdk)))
    if 'Q2_X2000_CC_ARCH' in env:
        env.pop('CFLAGS_mipsel_q2_os_linux_gnu')
    cargo = ['cargo', '+' + RUST_TOOLCHAIN]
    target = ['--release', '--offline', '--locked', '-Zbuild-std=std,panic_abort',
              '-Zbuild-std-features=panic-unwind,backtrace,backtrace-trace-only', '-Zjson-target-spec', '--target', spec]
    return dict(commit=commit, record=record, source_inputs=source_inputs, target_dir=target_dir, env=env,
                cargo=cargo, target=target, prefix=prefix, readelf=readelf)


def q2(a):
    out = a.out
    build = q2_environment(a)
    commit, record, source_inputs, target_dir = (build[k] for k in ('commit', 'record', 'source_inputs', 'target_dir'))
    prefix, readelf = build['prefix'], build['readelf']
    run([*build['cargo'], 'build', *build['target']], cwd=HERE, env=build['env'])
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


# Tests that start the test binary again as a child process; qemu-user cannot
# start a MIPS child without a binfmt handler, so q2-test skips them.
SELF_EXEC_TESTS = ('timer_wakeups::tests::timers_wake_the_runtime_thread_once_per_expiry',)


def q2_test(a):
    """Build the daemon's tests for the Q2 (with --profile-use, as the release is built)
    and run them under qemu-mipsel; also the dataplane's TSMP reject and transport queue tests."""
    build = q2_environment(a)
    sysroot = a.sdk / 'mipsel-buildroot-linux-gnu/sysroot'
    # libtest catches panics, so the test harness unwinds (cargo ignores
    # panic = "abort" for tests); abort-mode tests would need a child process
    # per test, which qemu-user cannot start. Everything else is the release build.
    target = ['-Zbuild-std=std' if arg == '-Zbuild-std=std,panic_abort' else arg for arg in build['target']]
    results = {}
    for name, args in (('tsrsd', ['--bin', NAME]), ('ts_dataplane', ['-p', 'ts_dataplane', '--lib'])):
        output = subprocess.run([*build['cargo'], 'test', '--no-run', '--message-format=json-render-diagnostics', *args, *target],
                                cwd=HERE, env=dict(build['env'], CARGO_PROFILE_RELEASE_PANIC='unwind'), check=True,
                                stdout=subprocess.PIPE, text=True).stdout
        binaries = [message['executable'] for message in map(json.loads, output.splitlines())
                    if message.get('reason') == 'compiler-artifact' and message.get('executable')
                    and message.get('profile', {}).get('test')]
        if len(binaries) != 1:
            raise SystemExit(f'{name}: expected one test executable, found {binaries}')
        filter_args = ['tsmp_reject', 'async_tokio'] if name == 'ts_dataplane' else [
            arg for test in SELF_EXEC_TESTS for arg in ('--skip', test)]
        process = subprocess.run([str(a.qemu_user), '-cpu', 'P5600', '-L', str(sysroot), binaries[0], *filter_args,
                                  '--test-threads=1'], capture_output=True, text=True, timeout=1800)
        sys.stdout.write(process.stdout[-3000:])
        summary = [line for line in process.stdout.splitlines() if line.startswith('test result:')]
        if process.returncode or len(summary) != 1:
            sys.stderr.write(process.stderr[-3000:])
            raise SystemExit(f'{name} tests failed under qemu-mipsel')
        passed = int(summary[0].split(' passed')[0].rsplit(' ', 1)[-1])
        results[name] = {'passed': passed, 'summary': summary[0], 'executable_sha256': sha(binaries[0]),
                         'skipped_self_exec': list(SELF_EXEC_TESTS) if name == NAME else []}
    record = dict(passed=True, target='q2-tests', panic='unwind', source_commit=build['commit'], pgo=build['record']['pgo'],
                  qemu_user_sha256=sha(a.qemu_user), results=results)
    a.out.mkdir(parents=True, exist_ok=False)
    (a.out / 'result.json').write_text(json.dumps(record, indent=2) + '\n')
    print(json.dumps({name: value['summary'] for name, value in results.items()}))


def admit(out):
    """A Q2 build output a release may ship: its record passed, its binary and
    receipts are the recorded ones, and its inputs are this tree's."""
    out = Path(out)
    record = json.loads((out / 'result.json').read_text())
    binary = out / NAME
    if (record.get('passed') is not True or record.get('target') != 'q2' or record.get('profile', 'release') != 'release'
            or binary.is_symlink() or not binary.is_file()
            or record.get('q2_sha256') != sha(binary) or record.get('symbols_receipt_sha256') != sha(out / 'symbols/receipt.json')
            or record.get('size_sha256') != sha(out / 'size.json')):
        raise ValueError('tsrsd must be the binary of a passing build.py q2 output')
    if record.get('inputs') != inputs() or record.get('tailscale_rs_commit') != TSRS_COMMIT or record.get('patch_sha256') != sha(PATCH):
        raise ValueError("tsrsd build inputs differ from this checkout")
    raw = binary.read_bytes()
    if raw[:6] != b'\x7fELF\x01\x01' or raw[18:20] != b'\x08\x00':
        raise ValueError('tsrsd must be a little-endian ELF32 MIPS binary')
    return dict(sha256=record['q2_sha256'], bytes=record['bytes'], source_commit=record['source_commit'],
                tailscale_rs_commit=TSRS_COMMIT, patch_sha256=record['patch_sha256'], record_sha256=sha(out / 'result.json'),
                profile_sha256=(record.get('pgo') or {}).get('profile_sha256'))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    actions = p.add_subparsers(dest='action', required=True)
    prep = actions.add_parser('prepare')
    prep.add_argument('--tailscale-source', type=lambda path: Path(path).resolve(),
                      help='local Git repository containing the pinned upstream commit')
    actions.add_parser('fetch')
    actions.add_parser('test')
    actions.add_parser('upstream-test')
    for action in ('host', 'q2', 'q2-test', 'admit'):
        command = actions.add_parser(action)
        command.add_argument('--out', type=lambda path: Path(path).resolve(), required=True)
    for action in ('q2', 'q2-test'):
        command = actions.choices[action]
        for name in ('sdk', 'toolchain-repo', 'compiler-work', 'rust-link'):
            command.add_argument('--' + name, type=lambda path: Path(path).resolve(), required=True)
        command.add_argument('--profile-use', type=lambda path: Path(path).resolve(),
                             help='PGO profile; its receipt.json beside it must match this build')
    actions.choices['q2'].add_argument('--profile-generate', type=lambda path: Path(path).resolve(), metavar='PROFILES',
                                       help='build an instrumented daemon that writes raw profiles to PROFILES (tools/pgo.py)')
    actions.choices['q2-test'].add_argument('--qemu-user', type=lambda path: Path(path).resolve(), required=True)
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
        run(['cargo', 'test', '--offline', '--locked', '-p', 'ts_dataplane', '--lib', '--', 'tsmp_reject', 'async_tokio'],
            cwd=HERE, env=env)
    elif a.action == 'upstream-test':
        upstream_tests()
    elif a.action == 'admit':
        print(json.dumps(admit(a.out)))
    elif a.out.exists() or a.out.is_symlink():
        p.error('--out requires an unused directory')
    elif a.action == 'host':
        host(a.out)
    elif a.action == 'q2-test':
        q2_test(a)
    else:
        q2(a)


if __name__ == '__main__':
    with cores.watched():
        main()
