#!/usr/bin/env bash
set -euo pipefail

log_path="${RUNNER_TEMP}/tel.log"
echo "log_path=${log_path}" >> "${GITHUB_OUTPUT}"

args=(run --config "${RUNNER_TEMP}/tasks.toml" --session-path "${SESSION_PATH}")
if [[ -n "${TASK_NAME:-}" ]]; then
  args+=(--task "${TASK_NAME}")
fi

tel "${args[@]}" 2>&1 | tee "${log_path}" | awk -v dim=$'\033[2m' -v debug=$'\033[34mDEBUG\033[0m' '
  index($0, dim) == 1 { drop = index($0, debug) > 0 }
  !drop { print; fflush() }
'
