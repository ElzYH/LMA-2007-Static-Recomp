#!/usr/bin/env bash
# ffmpeg_min.sh <out dir> [ffmpeg version]: FFmpeg's XMA decoders as static libraries, and nothing else
# (Route 1 for the release, the maintainer 22 Oct): libavcodec with xma1/xma2 (and the wmapro code they share)
# plus libavutil; no programs, formats, devices, filters, network or external libraries.
# LGPL 2.1 or later: no GPL or nonfree parts are enabled.
#
# Build lma-app / lma-cli against it with FFMPEG_DIR=<out dir> FFMPEG_STATIC=1 --features xma-ffmpeg;
# the exe then needs no FFmpeg DLLs. The same script is the LGPL relinking recipe in the release zip:
# rebuild these libraries (from this or a modified FFmpeg source) and relink with that cargo command.
#
# Windows: run in an MSYS2 UCRT64 shell (gcc, make); macOS / Linux: any shell with a C compiler.
# No nasm needed (--disable-x86asm: the decoders are C only).
set -euo pipefail
mkdir -p "${1:?usage: ffmpeg_min.sh <out dir> [version]}"
out=$(cd "$1" && pwd)  # absolute (macOS realpath has no -m)
ver=${2:-8.1}
work=$out.build
mkdir -p "$work"
cd "$work"
[ -f "ffmpeg-$ver.tar.xz" ] || curl -sSfLO "https://ffmpeg.org/releases/ffmpeg-$ver.tar.xz"
[ -d "ffmpeg-$ver" ] || tar -xJf "ffmpeg-$ver.tar.xz"
mkdir -p build
cd build
"../ffmpeg-$ver/configure" --prefix="$out" \
  --disable-everything --disable-programs --disable-doc --disable-network --disable-autodetect \
  --disable-avdevice --disable-avformat --disable-swresample --disable-swscale --disable-avfilter \
  --disable-x86asm --enable-static --disable-shared --enable-pic \
  --enable-decoder=xma1,xma2 --enable-parser=xma \
  --extra-cflags=-O2 > configure.log
grep -E '^License' configure.log
make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)" > make.log
make install > install.log
cp "../ffmpeg-$ver/COPYING.LGPLv2.1" "$out/"
echo "$ver" > "$out/VERSION"
ls -l "$out/lib"
