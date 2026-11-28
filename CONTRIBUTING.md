# Contributing to TSRS

Open an issue to discuss a larger change before starting. Pull requests
target `main`.

## Check a change locally

On Linux with Python 3, Git, Rust and a C toolchain, run the checks that CI
runs:

```sh
python3 test_release.py
python3 test_notices.py
python3 ci/test_smoke.py
python3 test_pgo.py
python3 build.py prepare
python3 build.py fetch
python3 build.py test
python3 tools/third_party_notices.py check
python3 build.py host --out build/host
python3 ci/smoke.py --tsrsd build/host/tsrsd
```

The [control test](README.md#control-test) needs unprivileged user
namespaces and takes about 100 s. A change to `tailscale-rs.patch` also
needs `python3 build.py upstream-test`. A change that affects the Q2 build
needs the [Q2 build](README.md#build-for-the-q2) and the smoke test under
`qemu-mipsel`.

## CI

The **Daemon checks** workflow runs on every pull request and push to
`main`. It runs the Python tests, the native daemon tests, the
[license check](README.md#third-party-notices), the smoke test of a host
build and the control test on a generated tailnet. A pull request
merges after review once these checks pass.

A daemon reaches the Q2 only as a tagged release. The **Build the Q2 network
daemon** workflow builds it against the SDK, compiler and emulator releases
in `release-lock.json`, profiles it, runs its tests and the smoke test under
`qemu-mipsel`, and publishes the prerelease.
[Release the target daemon](README.md#release-the-target-daemon)

## Commits

Commits on `main` must be signed. Write commit messages that state what the
change does. Keep personal data, local paths, tailnet names and addresses
out of commits, logs and test fixtures.

## License

Contributions are accepted under the repository's license,
`GPL-3.0-or-later`. Changes distributed in `tailscale-rs.patch` retain the
upstream project's license. [License](README.md#license)
