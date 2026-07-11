#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

release_tag="${1:-}"
workspace_version="$(sed -n '/^\[workspace\.package\]$/,/^\[/s/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
manifest_version="$(sed -n 's/.*"\."[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' .release-please-manifest.json)"

fail() {
  echo "CentaurAI Core release contract failed: $*" >&2
  exit 1
}

[[ -n "$workspace_version" ]] || fail "workspace package version is missing"
[[ "$manifest_version" == "$workspace_version" ]] \
  || fail "release-please version ${manifest_version:-<missing>} does not match Cargo version ${workspace_version}"

if [[ -n "$release_tag" ]]; then
  [[ "$release_tag" == "v${workspace_version}" ]] \
    || fail "tag ${release_tag} does not match Cargo version v${workspace_version}"
fi

bash scripts/check-centaurai-identity.sh
bash scripts/check-glibc-workflow-config.test.sh

release_workflow=".github/workflows/release.yml"
required_targets=(
  x86_64-unknown-linux-gnu
  aarch64-unknown-linux-gnu
  x86_64-apple-darwin
  aarch64-apple-darwin
  x86_64-pc-windows-msvc
  aarch64-pc-windows-msvc
)

for target in "${required_targets[@]}"; do
  grep -Fq "target: ${target}" "$release_workflow" \
    || fail "release target ${target} is missing"
done

target_count="$(grep -Ec '^[[:space:]]+target: (x86_64|aarch64)-' "$release_workflow")"
[[ "$target_count" == "6" ]] || fail "expected 6 release targets, found ${target_count}"

grep -Fq 'BINARY_NAME: centaurai-core' "$release_workflow" \
  || fail "canonical binary name is missing"
grep -Fq 'binary_name: centaurai-core.exe' "$release_workflow" \
  || fail "Windows binary name is missing"
grep -Fq 'ARCHIVE="centaurai-core-v${VERSION}-${{ matrix.target }}.tar.gz"' "$release_workflow" \
  || fail "Unix archive naming contract is missing"
grep -Fq '$archive = "centaurai-core-v${env:VERSION}-${{ matrix.target }}.zip"' "$release_workflow" \
  || fail "Windows archive naming contract is missing"
grep -Fq 'sha256sum centaurai-core-* > centaurai-core-checksums.txt' "$release_workflow" \
  || fail "checksum asset generation is missing"

echo "CentaurAI Core release contract passed for v${workspace_version}"
