#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GPG_DECRYPT_KEY:-}" ]]; then
  decrypted_session_path="${RUNNER_TEMP}/tel-session/session.sqlite"
  decrypted_session_dir="$(dirname "${decrypted_session_path}")"
  mkdir -p "${decrypted_session_dir}"

  if [[ ! -f "${INPUT_SESSION_PATH}" ]]; then
    echo "Encrypted session file not found: ${INPUT_SESSION_PATH}" >&2
    exit 1
  fi

  cp "${INPUT_SESSION_PATH}" "${decrypted_session_path}"
  GPG_PASS_KEY="${GPG_DECRYPT_KEY}" "${GITHUB_ACTION_PATH}/scripts/decrypt.sh" "${decrypted_session_path}"

  echo "SESSION_PATH=${decrypted_session_path}" >> "${GITHUB_ENV}"
  echo "DECRYPTED_SESSION_PATH=${decrypted_session_path}" >> "${GITHUB_ENV}"
else
  if [[ ! -f "${INPUT_SESSION_PATH}" ]]; then
    echo "Session file not found: ${INPUT_SESSION_PATH}" >&2
    exit 1
  fi
  echo "SESSION_PATH=${INPUT_SESSION_PATH}" >> "${GITHUB_ENV}"
fi
