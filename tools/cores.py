"""Notice core dumps from a run, keep them out of the source tree, and fail the run.

qemu-user writes a guest core (`qemu_<program>_<time>_<pid>.core`) into the
working directory of the crashed process, and the core carries the host's
environment. A script wraps its run in `watched()` and passes each working
directory it creates to `watch()`, or wraps a temporary one in `swept()`. Cores found in the
repository root, the current directory or a swept directory are moved to
`build/cores/` (ignored, never committed), reported with the crashed program
and its size, and the run exits non-zero so a crash is investigated rather
than lost.
"""
import contextlib
import re
import shutil
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEST = ROOT / 'build/cores'
CORE = re.compile(r'^(qemu_(?P<program>.+?)_\d{8}-\d{6}_(?P<pid>\d+)\.core|core(\.\d+)?|vgcore\.\d+)$')
found = []
_watched = []


def watch(directory):
    """Sweep this directory again when the run ends (for directories that outlive the run)."""
    _watched.append(Path(directory))


@contextlib.contextmanager
def swept(directory):
    """Sweep a temporary directory on leaving the block, before the directory is removed."""
    try:
        yield Path(directory)
    finally:
        sweep(directory)


def sweep(*directories, dest=None):
    """Move the cores in these directories (not their subdirectories) to dest (build/cores) and report each."""
    dest = Path(dest) if dest else DEST
    moved = []
    for directory in map(Path, directories):
        if not directory.is_dir():
            continue
        for path in sorted(directory.iterdir()):
            match = CORE.match(path.name)
            if not match or not path.is_file() or path.is_symlink():
                continue
            dest.mkdir(parents=True, exist_ok=True)
            target = dest / path.name
            if target.exists():
                target = dest / f'{time.strftime("%Y%m%d-%H%M%S")}-{path.name}'
            size = path.stat().st_size
            shutil.move(str(path), target)
            program = match.group('program') or 'unknown program'
            print(f'core dump: {program} (pid {match.group("pid") or "?"}, {size} bytes) moved to {target}. '
                  f'Investigate it with gdb-multiarch and the matching binary, then delete it.', file=sys.stderr)
            moved.append(target)
    found.extend(moved)
    return moved


@contextlib.contextmanager
def watched():
    """Run a script body; afterwards sweep the root, the current directory and watched directories,
    and exit non-zero if any core appeared."""
    message = 'core dump(s) during this run: see build/cores/'
    try:
        yield
    except SystemExit as exit:
        sweep(ROOT, Path.cwd(), *_watched)
        if found and exit.code in (0, None):
            raise SystemExit(f'{len(found)} {message}') from None
        raise
    except BaseException:
        sweep(ROOT, Path.cwd(), *_watched)
        raise
    sweep(ROOT, Path.cwd(), *_watched)
    if found:
        raise SystemExit(f'{len(found)} {message}')
