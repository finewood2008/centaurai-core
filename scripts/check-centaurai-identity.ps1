$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $repoRoot

function Require-Text {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Text
    )

    if (-not (Select-String -LiteralPath $Path -SimpleMatch $Text -Quiet)) {
        throw "CentaurAI identity check failed: '$Text' missing from $Path"
    }
}

Require-Text "Cargo.toml" 'license = "Apache-2.0"'
Require-Text "Cargo.toml" 'service = "centaurai-core"'
Require-Text "crates/aionui-app/Cargo.toml" 'name = "centaurai-core"'
Require-Text "crates/aionui-app/src/cli.rs" 'name = "centaurai-core"'
Require-Text "crates/aionui-app/src/router/health.rs" 'service: "centaurai-core"'
Require-Text "crates/aionui-app/src/commands/cmd_server.rs" '"AIONCORE_LISTENING"'
Require-Text "release-please-config.json" '"package-name": "centaurai-core"'
Require-Text ".github/workflows/release.yml" "BINARY_NAME: centaurai-core"

$releaseConfig = Get-Content ".github/workflows/release.yml", ".github/workflows/build-manual.yml" -Raw
if ($releaseConfig -match '(^|["/:-])aioncore(-v|-manual|\.exe|$)') {
    throw "CentaurAI identity check failed: a release artifact still uses the legacy aioncore name"
}

Write-Output "CentaurAI Core identity check passed"
