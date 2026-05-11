#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: GPG_PASS_KEY=<passphrase> $0 <file-path>" >&2
}

if [[ $# -ne 1 ]]; then
  usage
  exit 2
fi

file_path="$1"

if [[ -z "${GPG_PASS_KEY:-}" ]]; then
  echo "GPG_PASS_KEY is required." >&2
  exit 2
fi

if [[ ! -f "${file_path}" ]]; then
  echo "File not found: ${file_path}" >&2
  exit 1
fi

file_dir="$(dirname "${file_path}")"
tmp_output="$(mktemp "${file_dir}/file.encrypt.XXXXXX")"
cleanup() {
  rm -f "${tmp_output}"
}
trap cleanup EXIT

printf '%s' "${GPG_PASS_KEY}" | gpg \
  --batch \
  --yes \
  --pinentry-mode loopback \
  --passphrase-fd 0 \
  --symmetric \
  --cipher-algo AES256 \
  --output "${tmp_output}" \
  "${file_path}"

chmod 600 "${tmp_output}"
mv "${tmp_output}" "${file_path}"
trap - EXIT
