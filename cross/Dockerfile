# Cross-compiler for Raspberry Pi OS (64-bit, Debian bookworm): builds oftd for aarch64 on
# an x86 machine. Same Debian release as the Pi, so the binary links against a glibc the Pi
# has. Used by cross/build-pi.sh. GPL-3.0-or-later.
FROM rust:1-bookworm
RUN apt-get update \
    && apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add aarch64-unknown-linux-gnu
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
