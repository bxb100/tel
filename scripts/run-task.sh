#!/usr/bin/env bash
set -euo pipefail

args=(run --config "${RUNNER_TEMP}/tasks.toml" --session-path "${SESSION_PATH}")
if [[ -n "${TASK_NAME:-}" ]]; then
  args+=(--task "${TASK_NAME}")
fi

tel "${args[@]}"
