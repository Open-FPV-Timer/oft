#!/usr/bin/env bash
# Build oftd for a 64-bit Raspberry Pi (Pi OS bookworm) in Docker; no cross toolchain needed
# on this machine.
#
#   cross/build-pi.sh                 release build
#   cross/build-pi.sh user@pi[:dir]   also copy the binary to the Pi (default dir: ~)
#
# Output: target/aarch64-unknown-linux-gnu/release/oftd
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
IMAGE=oft-cross-pi
docker build -q -t "$IMAGE" cross >/dev/null
# As the calling user, so target/ stays ours; cargo's download cache lives in target/ too.
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD":/work -w /work \
    -e CARGO_HOME=/work/target/cross-cargo-home "$IMAGE" \
    cargo build --release -p oftd --target aarch64-unknown-linux-gnu
OUT=target/aarch64-unknown-linux-gnu/release/oftd
ls -l "$OUT"
if [ $# -ge 1 ]; then
    scp "$OUT" "$1$([[ $1 == *:* ]] || echo :)"
fi
