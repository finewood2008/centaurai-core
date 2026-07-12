#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

failed=0
while IFS= read -r action_ref; do
  if [[ ! "$action_ref" =~ @[0-9a-f]{40}([[:space:]]|$) ]]; then
    echo "Workflow action is not pinned to a full commit: ${action_ref}" >&2
    failed=1
  fi
done < <(sed -n 's/^[[:space:]]*uses:[[:space:]]*//p' .github/workflows/*.yml)

[[ "$failed" == 0 ]] || exit 1
echo "All workflow actions are pinned to full commits"
