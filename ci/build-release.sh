#!/usr/bin/env bash
# Run with the tagged q2-os and q2-toolchain checkouts and the release assets the workflow downloads under build/.
set -euo pipefail
root=$(pwd)
sdk_name=mipsel-buildroot-linux-gnu_sdk-buildroot
sdk="$root/build/$sdk_name"
compiler="$root/build/compiler"
os="$root/build/os-source"
recipe="$root/build/compiler-source"
python3 "$os/os/sdk_release.py" verify --bundle build/sdk-release --os-commit "$(git -C "$os" rev-parse HEAD)"
(cd build/compiler-release && sha256sum --check --strict compiler.tar.gz.sha256)
(cd build/qemu-release && sha256sum --check --strict qemu-linux-x86_64.tar.gz.sha256)
qemu="$root/build/qemu"
mkdir "$qemu"
tar -xzf build/qemu-release/qemu-linux-x86_64.tar.gz -C "$qemu"
[[ $(sha256sum "$qemu/bin/qemu-mipsel" | cut -d ' ' -f1) == $(jq -er '.files["bin/qemu-mipsel"].sha256' "$qemu/qemu-artifact.json") ]]
mkdir "$compiler"
tar -xzf build/compiler-release/compiler.tar.gz -C "$compiler"
jq -e --arg commit "$(git -C "$recipe" rev-parse HEAD)" '.producer_commit == $commit' "$compiler/compiler-bundle.json"
tar -xzf "build/sdk-release/$sdk_name.tar.gz" -C build
"$sdk/relocate-sdk.sh"
nightly=$(jq -er .rust_toolchain "$recipe/q2/toolchain/llvm/pin.json")
rustup toolchain install "$nightly" --profile minimal
export RUSTUP_TOOLCHAIN="$nightly"
python3 test_release.py
python3 ci/test_smoke.py
python3 test_pgo.py
python3 build.py fetch
python3 build.py test
# The target producer builds std offline. Populate its separately locked registry dependencies.
cargo fetch --locked --manifest-path "$compiler/install/lib/rustlib/src/rust/library/Cargo.toml"
inputs=(--sdk "$sdk" --toolchain-repo "$recipe" --compiler-work "$compiler" --rust-link "$os/os/rust-link.py")
# Profile on the generated tailnet, then build with the profile and run the
# daemon and dataplane tests built the same way under qemu-mipsel.
python3 tools/pgo.py profile "${inputs[@]}" --qemu-user "$qemu/bin/qemu-mipsel" --out build/pgo
python3 build.py q2 "${inputs[@]}" --profile-use build/pgo/tsrsd.profdata --out build/q2
jq -e '.profile == "release" and .pgo.receipt.program == "tsrsd" and .features == {"tsrsd": []}' build/q2/result.json
python3 build.py q2-test "${inputs[@]}" --profile-use build/pgo/tsrsd.profdata \
  --qemu-user "$qemu/bin/qemu-mipsel" --out build/q2-tests
jq -e '.results.tsrsd.passed >= 37 and .results.ts_dataplane.passed > 0' build/q2-tests/result.json
cp build/q2-tests/result.json build/q2-tests.json
python3 release.py pack --build build/q2 --archive build/tsrsd-q2.tar.gz
python3 release.py verify --archive build/tsrsd-q2.tar.gz
# Start the shipped binary under qemu-mipsel and check its offline status.
python3 ci/smoke.py --tsrsd build/q2/tsrsd --qemu "$qemu/bin/qemu-mipsel" \
  --sysroot "$sdk/mipsel-buildroot-linux-gnu/sysroot"
(cd build && sha256sum tsrsd-q2.tar.gz > tsrsd-q2.tar.gz.sha256)

# Ship the locked registry sources and their notices, plus the patched dependency.
cargo vendor --locked --offline --versioned-dirs build/vendor > build/vendor-config.toml
tar -czf build/third-party-source.tar.gz --exclude=.git \
  COPYING LICENSES Cargo.lock tailscale-rs.patch -C build vendor vendor-config.toml tailscale-rs
