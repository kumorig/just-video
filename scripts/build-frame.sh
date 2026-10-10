#!/usr/bin/env bash
# Cross-compile just-video for Steam Frame (aarch64, glibc 2.39) with cargo-zigbuild.
# FFmpeg + dav1d are linked statically from scripts/build-frame-media.sh, so the
# binary needs nothing from SteamOS beyond glibc and the kernel's V4L2 decoder.
set -euo pipefail
cd "$(dirname "$0")/.."
media=$PWD/.local-deps/frame-media
[ -f "$media/lib/libavcodec.a" ] || { echo "Run scripts/build-frame-media.sh first" >&2; exit 1; }
export PATH="$PWD/.local-deps/zig:$PATH"
t=aarch64_unknown_linux_gnu
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_LIBDIR_$t="$media/lib/pkgconfig"
export PKG_CONFIG_PATH_$t="$media/lib/pkgconfig"
export JUST_VIDEO_STATIC_FFMPEG=1
export CFLAGS_$t="-mcpu=cortex_a720 -O3"
# Rust code tuned for the Frame's Cortex-A720 cores too (the C side is above).
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=cortex-a720"
exec cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.39 "$@"
