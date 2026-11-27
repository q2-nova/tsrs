"""Tests of tools/cores.py on generated files."""
import contextlib
import io
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / 'tools'))
import cores  # noqa: E402


class CoresTests(unittest.TestCase):
    def setUp(self):
        cores.found.clear()
        cores._watched.clear()

    def test_a_qemu_core_is_moved_reported_and_fails_the_run(self):
        with tempfile.TemporaryDirectory() as work, tempfile.TemporaryDirectory() as dest:
            core = Path(work, 'qemu_tsrsd_20260101-120000_4242.core')
            core.write_bytes(b'\x7fELF generated')
            Path(work, 'tsrsd.log').write_text('kept')
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                moved = cores.sweep(work, dest=Path(dest))
            self.assertEqual([p.name for p in moved], [core.name])
            self.assertFalse(core.exists())
            self.assertTrue(Path(work, 'tsrsd.log').exists())
            self.assertIn('core dump: tsrsd (pid 4242, 14 bytes)', err.getvalue())

    def test_watched_exits_non_zero_after_a_core(self):
        cores.found.append(Path('generated.core'))
        with self.assertRaises(SystemExit) as exit, contextlib.redirect_stderr(io.StringIO()):
            with cores.watched():
                pass
        self.assertIn('1 core dump(s)', str(exit.exception))

    def test_a_successful_sys_exit_still_fails_after_a_core(self):
        cores.found.append(Path('generated.core'))
        with self.assertRaises(SystemExit) as exit, contextlib.redirect_stderr(io.StringIO()):
            with cores.watched():
                sys.exit(0)
        self.assertIn('1 core dump(s)', str(exit.exception))

    def test_watched_keeps_the_original_failure(self):
        cores.found.append(Path('generated.core'))
        with self.assertRaises(ValueError), contextlib.redirect_stderr(io.StringIO()):
            with cores.watched():
                raise ValueError('original')

    def test_swept_runs_before_a_temporary_directory_is_removed(self):
        with contextlib.redirect_stderr(io.StringIO()):
            with tempfile.TemporaryDirectory() as dest:
                cores_dest = Path(dest)
                original = cores.DEST
                cores.DEST = cores_dest
                try:
                    with tempfile.TemporaryDirectory() as tmp, cores.swept(tmp):
                        Path(tmp, 'core.77').write_bytes(b'generated')
                    self.assertTrue((cores_dest / 'core.77').exists())
                finally:
                    cores.DEST = original

    def test_other_files_are_left_alone(self):
        with tempfile.TemporaryDirectory() as work:
            for name in ('core.rs', 'cores.py', 'score.core.txt'):
                Path(work, name).write_text('x')
            self.assertEqual(cores.sweep(work, dest=Path(work, 'dest')), [])


if __name__ == '__main__':
    unittest.main()
