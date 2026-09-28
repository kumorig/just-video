#!/usr/bin/env bash
# Builds the media stack bundled into the Steam Frame binary: static dav1d and a
# minimal static FFmpeg, cross-compiled with zig for aarch64 (glibc 2.39) and
# tuned for the Frame's Cortex-A720 cores. Output: .local-deps/frame-media.
set -euo pipefail
cd "$(dirname "$0")/.."
FFMPEG=8.1.3 DAV1D=1.5.4 ZLIB=1.3.1
deps=$PWD/.local-deps
prefix=$deps/frame-media wrap=$deps/zig-wrap src=$deps/src
zig=$deps/zig/zig
jobs=$(getconf _NPROCESSORS_ONLN)
mkdir -p "$src" "$wrap"

# Toolchain: zig as cross compiler, meson in a local venv (dav1d only).
# zig runs on the build machine: Linux or macOS, x86_64 or arm64.
if [ ! -x "$zig" ]; then
  mkdir -p "$deps/zig"
  case "$(uname -s)" in Darwin) host_os=macos ;; *) host_os=linux ;; esac
  host_arch=$(uname -m); [ "$host_arch" = arm64 ] && host_arch=aarch64
  curl -sL "https://ziglang.org/download/0.14.1/zig-$host_arch-$host_os-0.14.1.tar.xz" |
    tar xJ -C "$deps/zig" --strip-components=1
fi
for tool in cc c++; do
  printf '#!/bin/sh\nexec %s %s -target aarch64-linux-gnu.2.39 -mcpu=cortex_a720 "$@"\n' \
    "$zig" "$tool" > "$wrap/aarch64-$tool"
done
for tool in ar ranlib; do printf '#!/bin/sh\nexec %s %s "$@"\n' "$zig" "$tool" > "$wrap/aarch64-$tool"; done
chmod +x "$wrap"/*
if [ ! -x "$deps/buildtools/bin/meson" ] || [ ! -x "$deps/buildtools/bin/ninja" ]; then
  if command -v uv >/dev/null; then
    uv venv -q --allow-existing "$deps/buildtools" && uv pip install -q --python "$deps/buildtools" meson ninja
  else
    python3 -m venv "$deps/buildtools" && "$deps/buildtools/bin/pip" install -q meson ninja
  fi
fi

[ -d "$src/dav1d-$DAV1D" ] || curl -sL "https://downloads.videolan.org/pub/videolan/dav1d/$DAV1D/dav1d-$DAV1D.tar.xz" | tar xJ -C "$src"
[ -d "$src/zlib-$ZLIB" ] || curl -sfL "https://github.com/madler/zlib/releases/download/v$ZLIB/zlib-$ZLIB.tar.gz" | tar xz -C "$src"
if [ ! -d "$src/ffmpeg-$FFMPEG" ]; then
  curl -sL "https://ffmpeg.org/releases/ffmpeg-$FFMPEG.tar.xz" | tar xJ -C "$src"
  for patch in "$PWD"/third_party/ffmpeg-patches/*.patch; do
    patch -d "$src/ffmpeg-$FFMPEG" -p1 < "$patch"
  done
fi

cat > "$deps/aarch64-cross.ini" <<INI
[binaries]
c = '$wrap/aarch64-cc'
cpp = '$wrap/aarch64-c++'
ar = '$wrap/aarch64-ar'
ranlib = '$wrap/aarch64-ranlib'
strip = 'true'

[host_machine]
system = 'linux'
cpu_family = 'aarch64'
cpu = 'cortex-a720'
endian = 'little'
INI

# zlib: Matroska tracks with zlib content compression. CHOST keeps its
# configure from switching to Apple's libtool when building on macOS.
(cd "$src/zlib-$ZLIB" &&
  CHOST=aarch64-linux-gnu CC="$wrap/aarch64-cc" AR="$wrap/aarch64-ar" RANLIB="$wrap/aarch64-ranlib" CFLAGS="-O3 -fPIC" \
    ./configure --static --prefix="$prefix" && make -j"$jobs" libz.a && make install)

(cd "$src/dav1d-$DAV1D" && rm -rf build && export PATH="$deps/buildtools/bin:$PATH" &&
  meson setup build --cross-file "$deps/aarch64-cross.ini" \
    --prefix="$prefix" --libdir=lib --buildtype=release --default-library=static \
    -Denable_tools=false -Denable_tests=false -Denable_asm=true &&
  ninja -C build install)

cd "$src/ffmpeg-$FFMPEG"
PKG_CONFIG_LIBDIR="$prefix/lib/pkgconfig" PKG_CONFIG_PATH= ./configure --prefix="$prefix" \
  --enable-cross-compile --arch=aarch64 --target-os=linux \
  --cc="$wrap/aarch64-cc" --cxx="$wrap/aarch64-c++" --ar="$wrap/aarch64-ar" --ranlib="$wrap/aarch64-ranlib" \
  --nm=nm --strip=true --pkg-config=pkg-config --pkg-config-flags=--static \
  --enable-static --disable-shared --enable-pic --disable-debug --disable-doc --disable-programs \
  --disable-network --disable-autodetect --enable-zlib --disable-avdevice --disable-avfilter --disable-swscale \
  --enable-swresample --disable-everything --enable-libdav1d --enable-v4l2-m2m \
  --enable-decoder=h264,hevc,vp9,libdav1d,h264_v4l2m2m,hevc_v4l2m2m,vp9_v4l2m2m,aac,aac_latm,ac3,eac3,opus,flac,mp3,mp2,vorbis,dca,truehd,alac,pcm_s16le,pcm_s24le,pcm_s32le,pcm_f32le,ass,ssa,subrip,srt,webvtt,movtext,text,dvdsub,pgssub,dvbsub \
  --enable-demuxer=mov,matroska,mpegts,avi \
  --enable-parser=h264,hevc,vp9,av1,aac,aac_latm,ac3,opus,flac,mpegaudio,dca,vorbis \
  --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,vp9_superframe_split,extract_extradata,av1_frame_split \
  --extra-cflags="-O3"
# zig's glibc stubs make configure think sys/sysctl.h exists; glibc removed it.
sed -i.orig 's/#define HAVE_SYSCTL 1/#define HAVE_SYSCTL 0/' config.h && rm config.h.orig
make -j"$jobs"
make install
echo "Frame media stack installed in $prefix"
