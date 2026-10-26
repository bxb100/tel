#!/usr/bin/env bash
set -euo pipefail

log_path="${RUNNER_TEMP}/tel.log"
echo "log_path=${log_path}" >> "${GITHUB_OUTPUT}"

args=(run --config "${RUNNER_TEMP}/tasks.toml" --session-path "${SESSION_PATH}")
if [[ -n "${TASK_NAME:-}" ]]; then
  args+=(--task "${TASK_NAME}")
fi

tel "${args[@]}" 2>&1 | tee "${log_path}" | sed -u $'/\x1b\\[34mDEBUG\x1b\\[0m/d'
