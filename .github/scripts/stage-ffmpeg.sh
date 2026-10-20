#!/usr/bin/env bash
set -euo pipefail

src="${1:?source ffmpeg required}"
dest_dir="${2:?destination directory required}"
name="$(basename "${src}")"

mkdir -p "${dest_dir}"
cp "${src}" "${dest_dir}/${name}"
chmod 755 "${dest_dir}/${name}"
