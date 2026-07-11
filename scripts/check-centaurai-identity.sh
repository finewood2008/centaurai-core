#!/usr/bin/env bash

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

require_text() {
    local file="$1"
    local text="$2"
    if ! grep -Fq "$text" "$file"; then
        echo "CentaurAI identity check failed: '$text' missing from $file" >&2
        exit 1
    fi
}

require_text Cargo.toml 'license = "Apache-2.0"'
require_text Cargo.toml 'service = "centaurai-core"'
require_text crates/aionui-app/Cargo.toml 'name = "centaurai-core"'
require_text crates/aionui-app/src/cli.rs 'name = "centaurai-core"'
require_text crates/aionui-app/src/router/health.rs 'service: "centaurai-core"'
require_text crates/aionui-app/src/commands/cmd_server.rs '"AIONCORE_LISTENING"'
require_text release-please-config.json '"package-name": "centaurai-core"'
require_text .github/workflows/release.yml 'BINARY_NAME: centaurai-core'

if grep -Eq '(^|["/:-])aioncore(-v|-manual|\.exe|$)' .github/workflows/release.yml .github/workflows/build-manual.yml; then
    echo "CentaurAI identity check failed: a release artifact still uses the legacy aioncore name" >&2
    exit 1
fi

echo "CentaurAI Core identity check passed"
