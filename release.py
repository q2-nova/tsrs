#!/usr/bin/env python3
"""Pack or verify the portable TSRS runtime input consumed by q2-os."""
import argparse
import gzip
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import re
import subprocess
import tarfile
import tempfile

FILES = ("tsrsd", "result.json", "size.json", "symbols/receipt.json", "licenses/THIRD-PARTY-NOTICES")


def read_json(path):
    return json.loads(path.read_bytes())


def dump(obj):
    return (json.dumps(obj, indent=2, sort_keys=True) + "\n").encode()


def sha(data):
    return hashlib.sha256(data).hexdigest()


def load_producer(source):
    spec = importlib.util.spec_from_file_location("daemon_build", source / "build.py")
    producer = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(producer)
    return producer


def admit(source, directory):
    producer = load_producer(source)
    admitted = producer.admit(directory)
    commit = subprocess.check_output(["git", "-C", source, "rev-parse", "HEAD^{commit}"], text=True).strip()
    if admitted["source_commit"] != commit:
        raise ValueError("daemon build differs from release source commit")
    return read_json(directory / "result.json")


def pack(source, directory, out):
    record = admit(source, directory)
    binary = (directory / "tsrsd").read_bytes()
    sizes = read_json(directory / "size.json")
    symbols = read_json(directory / "symbols/receipt.json")
    if (not symbols["passed"] or not symbols["debuglink_verified"] or not symbols["allocated_sections_unchanged"]
            or sizes["sha256"] != sha(binary) or symbols["artifacts"]["deployed"]["sha256"] != sha(binary)):
        raise ValueError("daemon size or symbols do not match")
    # Local diagnostics contain build paths. Carry measured values and original
    # receipt hashes into separate, portable reports for the OS consumer.
    size = {k: sizes[k] for k in ("deployed_bytes", "allocated_file_bytes", "allocated_zero_fill_bytes")}
    size.update(sha256=sha(binary), original_receipt_sha256=record["size_sha256"])
    sym = {name: symbols["artifacts"][name]["sha256"] for name in ("deployed", "debug", "unstripped")}
    sym.update(original_receipt_sha256=record["symbols_receipt_sha256"], debuglink_verified=True)
    runtime = {k: record[k] for k in ("bytes", "inputs", "passed", "target", "toolchain", "source_commit",
                                       "tailscale_rs_commit", "patch_sha256", "q2_sha256", "build_dependencies",
                                       "notices_sha256", "notices_tool")}
    if (not all(re.fullmatch("[0-9a-f]{64}", value) for value in record["build_dependencies"].values())
            or runtime["toolchain"] != load_producer(source).RUST_TOOLCHAIN):
        raise ValueError("unexpected build dependencies")
    # A profile-guided build carries its profile receipt (build.py q2 --profile-use).
    producer = load_producer(source)
    runtime.update(profile=record.get("profile", "release"), features=record.get("features", producer.Q2_FEATURES),
                   pgo=record.get("pgo"))
    runtime.update(size_sha256=sha(dump(size)), symbols_receipt_sha256=sha(dump(sym)))
    notices = (directory / "licenses/THIRD-PARTY-NOTICES").read_bytes()
    payload = dict(zip(FILES, (binary, dump(runtime), dump(size), dump(sym), notices)))
    out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=out.parent) as tmp:
        stage = Path(tmp) / "tsrsd-q2.tar.gz"
        with stage.open("xb") as raw, gzip.GzipFile(filename="", fileobj=raw, mode="wb", mtime=0) as gz:
            with tarfile.open(fileobj=gz, mode="w") as tar:
                for name, data in payload.items():
                    info = tarfile.TarInfo(name)
                    info.size = len(data)
                    info.mode = 0o755 if name == "tsrsd" else 0o644
                    tar.addfile(info, io.BytesIO(data))
        verify(source, stage)
        out.hardlink_to(stage)


def verify(source, archive):
    with tempfile.TemporaryDirectory() as tmp, tarfile.open(archive, "r:gz") as tar:
        directory = Path(tmp)
        names = set()
        for info in tar:
            if info.name not in FILES or info.name in names or not info.isfile() or info.size > 128 * 1024 * 1024:
                raise ValueError("unexpected daemon archive member")
            names.add(info.name)
            path = directory / info.name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(tar.extractfile(info).read())
        if names != set(FILES):
            raise ValueError("incomplete daemon archive")
        record = admit(source, directory)
        if record["bytes"] != (directory / "tsrsd").stat().st_size:
            raise ValueError("incorrect recorded length")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=Path(__file__).resolve().parent)
    actions = parser.add_subparsers(dest="command", required=True)
    package = actions.add_parser("pack")
    package.add_argument("--build", type=Path, required=True)
    for action in (package, actions.add_parser("verify")):
        action.add_argument("--archive", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "pack":
        pack(args.source.resolve(), args.build, args.archive)
    else:
        verify(args.source.resolve(), args.archive)
    print(sha(args.archive.read_bytes()), args.archive.name)


if __name__ == "__main__":
    main()
