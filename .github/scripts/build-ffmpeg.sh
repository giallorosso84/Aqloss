#!/usr/bin/env bash
# FFmpeg 7.1.1, LGPL-2.1-or-later. These flags are the source offer.
set -euo pipefail

out="${1:?output path required}"
host="${2:-linux}"
version="7.1.1"
sha256="733984395e0dbbe5c046abda2dc49a5544e7e0e1e2366bba849222ae9e3a03b1"

case "${host}" in
  linux | darwin | windows) ;;
  *)
    echo "host must be linux, darwin, or windows" >&2
    exit 1
    ;;
esac

mkdir -p "$(dirname "${out}")"
out="$(cd "$(dirname "${out}")" && pwd)/$(basename "${out}")"

workdir="$(mktemp -d)"
trap 'rm -rf "${workdir}"' EXIT

tarball="${workdir}/ffmpeg-${version}.tar.xz"
curl -fsSL "https://ffmpeg.org/releases/ffmpeg-${version}.tar.xz" -o "${tarball}"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${tarball}" | awk '{print $1}')"
else
  actual="$(shasum -a 256 "${tarball}" | awk '{print $1}')"
fi
if [ "${actual}" != "${sha256}" ]; then
  echo "ffmpeg tarball sha256 mismatch" >&2
  exit 1
fi
tar -xJ -C "${workdir}" -f "${tarball}"
cd "${workdir}/ffmpeg-${version}"

config=(
  --disable-everything
  --disable-autodetect
  --disable-doc
  --disable-network
  --disable-debug
  --disable-avdevice
  --disable-swscale
  --disable-postproc
  --disable-asm
  --enable-small
  --enable-static
  --disable-shared
  --enable-ffmpeg
  --disable-ffprobe
  --enable-avcodec
  --enable-avformat
  --enable-avutil
  --enable-swresample
  --enable-avfilter
  --enable-protocol=file,pipe
  --enable-demuxer=mov
  --enable-decoder=ac3,eac3
  --enable-muxer=pcm_f32le
  --enable-encoder=pcm_f32le
  --enable-filter=aresample
  --extra-cflags=-Os
)

if [ "${host}" = "windows" ]; then
  config+=(
    --arch=x86_64
    --target-os=mingw32
    --cross-prefix=x86_64-w64-mingw32-
    --extra-ldflags=-static
  )
fi

./configure "${config[@]}"
make -j"$(nproc)"
mkdir -p "$(dirname "${out}")"
if [ "${host}" = "windows" ]; then
  cp ffmpeg.exe "${out}"
else
  cp ffmpeg "${out}"
  chmod 755 "${out}"
  if command -v strip >/dev/null 2>&1; then
    strip "${out}" || true
  fi
fi
echo "Built ${out} ($(du -h "${out}" | cut -f1))"
