#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${TASK_DATA_URL:-}" ]]; then
  echo "TASK_DATA_URL is required." >&2
  exit 2
fi

curl \
  --fail \
  --location \
  --show-error \
  --silent \
  --output "${RUNNER_TEMP}/tasks.toml" \
  "${TASK_DATA_URL}"
