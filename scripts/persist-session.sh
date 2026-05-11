#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${GPG_DECRYPT_KEY:-}" ]]; then
  exit 0
fi

if [[ -z "${DECRYPTED_SESSION_PATH:-}" ]]; then
  echo "DECRYPTED_SESSION_PATH is required." >&2
  exit 2
fi

if [[ -z "${INPUT_SESSION_PATH:-}" ]]; then
  echo "INPUT_SESSION_PATH is required." >&2
  exit 2
fi

if [[ ! -f "${DECRYPTED_SESSION_PATH}" ]]; then
  echo "Decrypted session file not found: ${DECRYPTED_SESSION_PATH}" >&2
  exit 1
fi

target_dir="$(dirname "${INPUT_SESSION_PATH}")"
if [[ ! -d "${target_dir}" ]]; then
  echo "Encrypted session directory not found: ${target_dir}" >&2
  exit 1
fi

tmp_session="$(mktemp "${target_dir}/tel-session.XXXXXX")"
cleanup() {
  rm -f "${tmp_session}"
}
trap cleanup EXIT

cp "${DECRYPTED_SESSION_PATH}" "${tmp_session}"
chmod 600 "${tmp_session}"

GPG_PASS_KEY="${GPG_DECRYPT_KEY}" "${GITHUB_ACTION_PATH}/scripts/encrypt.sh" "${tmp_session}"
mv "${tmp_session}" "${INPUT_SESSION_PATH}"
trap - EXIT
