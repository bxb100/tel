#!/usr/bin/env bash
set -euo pipefail

if [[ -z "${TASK_PATH:-}" ]]; then
  echo "TASK_PATH is required." >&2
  exit 2
fi

if [[ ! $TASK_PATH =~ ^http ]]; then
    mv "$TASK_PATH" "$RUNNER_TEMP"
    exit 0
fi

curl \
  --fail \
  --location \
  --show-error \
  --silent \
  --output "${RUNNER_TEMP}/tasks.toml" \
  "${TASK_PATH}"
