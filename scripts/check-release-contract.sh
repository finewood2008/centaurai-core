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
bash scripts/check-workflow-actions-pinned.sh
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
grep -Fq 'centaurai-core-release.json' "$release_workflow" \
  || fail "release provenance manifest is missing"
grep -Fq '"commit": os.environ["SOURCE_COMMIT"]' "$release_workflow" \
  || fail "release manifest does not record the source commit"
grep -Fq 'CENTAURAI_CORE_LISTENING' "$release_workflow" \
  || fail "release workflow does not validate the launched binary listening contract"
grep -Fq '/api/capabilities' "$release_workflow" \
  || fail "release workflow does not query the launched binary capabilities"
grep -Fq 'centaurai-core-capabilities.json' "$release_workflow" \
  || fail "release capabilities asset is missing"
grep -Fq '"capabilities": capabilities_response["data"]' "$release_workflow" \
  || fail "release provenance does not embed the launched binary capabilities"
for required_feature in \
  decision_per_brain_knowledge_egress \
  knowledge_dispatch_egress_gate \
  knowledge_upload_idempotency_v2 \
  provider_ssrf_pinned_dns; do
  grep -Fq "\"${required_feature}\"" "$release_workflow" \
    || fail "release binary capability gate is missing ${required_feature}"
done

while IFS= read -r action_ref; do
  [[ "$action_ref" =~ @[0-9a-f]{40}([[:space:]]|$) ]] \
    || fail "release action is not pinned to a commit: ${action_ref}"
done < <(sed -n 's/^[[:space:]]*uses:[[:space:]]*//p' "$release_workflow")

echo "CentaurAI Core release contract passed for v${workspace_version}"
