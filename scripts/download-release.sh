#!/usr/bin/env bash
set -euo pipefail

if [[ "${RUNNER_OS}" != "Linux" ]]; then
  echo "This action currently supports Linux runners only." >&2
  exit 1
fi

release_repo="${ACTION_REPOSITORY:-${WORKFLOW_REPOSITORY}}"
release_dir="${RUNNER_TEMP}/tel-release"
archive="tel-linux-x86_64.tar.gz"
mkdir -p "${release_dir}"

tag="$(gh release list --repo "${release_repo}" --limit 1 --json tagName --jq '.[0].tagName')"
if [[ -z "${tag}" ]]; then
  echo "No GitHub release found for ${release_repo}." >&2
  exit 1
fi

gh release download "${tag}" \
  --repo "${release_repo}" \
  --pattern "${archive}" \
  --dir "${release_dir}"

tar -xzf "${release_dir}/${archive}" -C "${release_dir}"
chmod +x "${release_dir}/tel"
echo "${release_dir}" >> "${GITHUB_PATH}"
