"""Release archive controls, using a generated ELF header and build receipts."""
import hashlib
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

import build
import release

ROOT = Path(__file__).resolve().parent


class ReleaseArchive(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.dir = Path(temp.name)
        self.out = self.dir / "tsrsd-q2.tar.gz"
        (self.dir / "symbols").mkdir()
        (self.dir / "licenses").mkdir()
        self.notices = b"Third-party software notices: tsrsd\n"
        (self.dir / build.NOTICES).write_bytes(self.notices)
        binary = bytearray(52)
        binary[:7] = b'\x7fELF\x01\x01\x01'
        binary[16:20] = b'\x02\x00\x08\x00'
        self.digest = hashlib.sha256(binary).hexdigest()
        (self.dir / "tsrsd").write_bytes(binary)
        self.write("size.json", dict(sha256=self.digest, deployed_bytes=len(binary),
                                     allocated_file_bytes=0, allocated_zero_fill_bytes=0))
        self.write("symbols/receipt.json", dict(passed=True, debuglink_verified=True,
                    allocated_sections_unchanged=True, artifacts={k: {"sha256": self.digest}
                    for k in ("deployed", "unstripped", "debug")}))
        self.record = dict(passed=True, target="q2", toolchain=build.RUST_TOOLCHAIN, inputs=build.inputs(),
                           source_commit=subprocess.check_output(["git","-C",ROOT,"rev-parse","HEAD"],text=True).strip(),
                           tailscale_rs_commit=build.TSRS_COMMIT, patch_sha256=build.sha(build.PATCH),
                           build_dependencies={"sdk_gcc_sha256": self.digest}, q2_sha256=self.digest,
                           bytes=len(binary), symbols_receipt_sha256=build.sha(self.dir / "symbols/receipt.json"),
                           size_sha256=build.sha(self.dir / "size.json"),
                           notices_sha256=build.sha(self.dir / build.NOTICES), notices_tool="cargo-about 0.9.3")
        self.write("result.json", self.record)

    def write(self, name, obj):
        (self.dir / name).write_text(json.dumps(obj))

    def test_round_trip(self):
        release.pack(ROOT, self.dir, self.out)
        release.verify(ROOT, self.out)

    def test_notices_are_carried(self):
        release.pack(ROOT, self.dir, self.out)
        with tarfile.open(self.out) as archive:
            self.assertEqual(archive.extractfile(build.NOTICES).read(), self.notices)
            record = json.load(archive.extractfile("result.json"))
        self.assertEqual(record["notices_sha256"], hashlib.sha256(self.notices).hexdigest())
        self.assertEqual(record["notices_tool"], "cargo-about 0.9.3")

    def test_missing_or_changed_notices_refused(self):
        (self.dir / build.NOTICES).write_bytes(b"changed\n")
        with self.assertRaisesRegex(ValueError, "third-party notices"):
            release.pack(ROOT, self.dir, self.out)
        (self.dir / build.NOTICES).unlink()
        with self.assertRaisesRegex(ValueError, "third-party notices"):
            release.pack(ROOT, self.dir, self.out)
        self.assertFalse(self.out.exists())

    def test_profile_receipt_is_carried(self):
        receipt = dict(program="tsrsd", profile_sha256="1" * 64, training_version=1)
        self.record.update(profile="release", features=build.Q2_FEATURES,
                           pgo=dict(profile_sha256="1" * 64, receipt_sha256="2" * 64, receipt=receipt))
        self.write("result.json", self.record)
        release.pack(ROOT, self.dir, self.out)
        with tarfile.open(self.out) as archive:
            record = json.load(archive.extractfile("result.json"))
        self.assertEqual(record["pgo"]["receipt"], receipt)
        self.assertEqual(record["profile"], "release")

    def test_instrumented_build_refused(self):
        self.record["profile"] = "pgo-instrumented"
        self.write("result.json", self.record)
        with self.assertRaises(ValueError):
            release.pack(ROOT, self.dir, self.out)

    def test_changed_binary(self):
        (self.dir / "tsrsd").write_bytes(b"changed")
        with self.assertRaises(ValueError):
            release.pack(ROOT, self.dir, self.out)
        self.assertFalse(self.out.exists())

    def test_wrong_source(self):
        self.record["source_commit"] = "0" * 40
        self.write("result.json", self.record)
        with self.assertRaisesRegex(ValueError, "source commit"):
            release.pack(ROOT, self.dir, self.out)


if __name__ == "__main__":
    unittest.main()
