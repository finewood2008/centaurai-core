# Upstream maintenance

CentaurAI Core is an independently maintained fork of
[iOfficeAI/AionCore](https://github.com/iOfficeAI/AionCore). This document
defines how upstream changes enter the CentaurAI release line without making
product builds depend directly on upstream state.

## Current fork baseline

| Field | Value |
| --- | --- |
| Fork created | 2026-07-11 |
| Last synchronized | 2026-07-11 |
| Upstream tag | `v0.1.45` |
| Upstream commit | [`928f91c8981bb2475040ff05792f01940eaebc97`](https://github.com/iOfficeAI/AionCore/commit/928f91c8981bb2475040ff05792f01940eaebc97) |

This section is the repository's source of truth for the most recently
integrated upstream baseline. Every formal upstream sync must update the tag,
full commit hash, matching `[workspace.metadata.centaurai]` values in
`Cargo.toml`, and **Last synchronized** date in the same pull request. Keep
the original **Fork created** date unchanged. Record the previous and new
baselines in the sync pull request and CentaurAI release notes so the integration
history remains auditable.

## Remote ownership

Use these remote roles consistently:

- `origin` is the CentaurAI-owned repository and the only normal push target.
- `upstream` is `https://github.com/iOfficeAI/AionCore.git` and is fetch-only.

Verify the setup before synchronizing:

```bash
git remote -v
git remote get-url upstream
git remote get-url --push upstream
```

For a new clone, configure the upstream remote without enabling pushes:

```bash
git remote add upstream https://github.com/iOfficeAI/AionCore.git
git remote set-url --push upstream no_push
git fetch upstream --prune --tags
```

Never push CentaurAI branches or tags to the upstream repository. Do not change
a developer's `origin` to point at AionCore.

## Synchronization policy

Upstream is an input, not an automatic deployment source. Every intake goes
through a review branch and a CentaurAI pull request:

1. Start from a clean, current CentaurAI `main` branch and fetch `upstream` with
   pruning and tags.
2. Record the upstream commit or release tag being evaluated, then create a
   `sync/aioncore-<version-or-date>` branch.
3. Inspect the full upstream range before integrating it. Pay particular
   attention to API types, route registration, WebSocket events, migrations,
   authentication, subprocess/runtime packages, dependencies, licensing, and
   bundled assets.
4. Merge the selected upstream commit into the sync branch. Prefer a merge that
   preserves both histories; do not rebase already published CentaurAI history.
   A narrowly scoped security or correctness fix may be cherry-picked when a
   full sync is unsuitable, but its upstream commit must be recorded.
5. Resolve conflicts in favor of the CentaurAI product contract. Do not accept
   upstream versions of a conflicted file mechanically. Document intentional
   deviations that future syncs will encounter again.
6. Run the full repository checks and the consumer compatibility matrix. Review
   generated artifacts and database migrations before approving the pull
   request.
7. Release a CentaurAI-owned version, then update products through their normal
   staged rollout. Products never pin `upstream/main`, an upstream-only tag, or
   an unreviewed sync branch.

No scheduled job should automatically merge upstream, publish a core artifact,
or update a consuming product. Automation may fetch, compare, open an intake
issue, and report security advisories; a maintainer still approves integration
and release.

## Compatibility gates

An upstream intake cannot ship until the following surfaces are checked:

| Surface | CentaurAI commitment | Required review |
| --- | --- | --- |
| REST | Preserve supported `/api/*` routes, methods, status codes, and response semantics. Prefer additive fields and endpoints. | Contract tests for every consuming product; version and migrate intentional breaks. |
| WebSocket | Preserve `/ws`, event names, ordering assumptions, and payload compatibility used by released hosts. | Replay/stream tests and an inventory of affected subscribers. |
| SQLite | Open and migrate supported AionCore/CentaurAI data directories forward without silent loss. Existing migrations remain immutable. | Backup/restore and migration tests using production-shaped fixtures; audit direct-table bridge consumers. |
| Startup | Continue emitting one newline-terminated `AIONCORE_LISTENING <json>` record after a successful bind. The JSON retains string `host` and numeric `port` fields. | Launch on a dynamic port, parse stdout, and verify `/health`. |
| Process lifecycle | Preserve supported launch flags, parent-process shutdown, and graceful data-layer close behavior used by hosts. | Embedded-host launch, shutdown, crash-recovery, and upgrade tests. |

The legacy `AIONCORE_LISTENING` prefix is intentionally stable even though the
product brand is CentaurAI Core. A future CentaurAI-named readiness record may
be emitted in addition to it, but the legacy record must not be removed or
changed until every supported host has migrated through a published deprecation
window.

SQLite compatibility means supported databases migrate forward through
reviewed, append-only migrations. It does not make every table a permanent
public interface. If a CentaurAI product currently reads SQLite directly, list
it as an affected consumer and coordinate its change before altering that
schema. Never test an intake by pointing multiple running binaries at one data
directory, and always preserve a restorable backup before exercising a
destructive or irreversible migration.

## Multi-product release discipline

CentaurAI Core has one release line shared by multiple products. Keep that
possible by following these rules:

- Build signed, reproducible artifacts for each supported platform from a
  CentaurAI commit and attach an immutable version and digest.
- Maintain a product compatibility matrix covering the core version, host
  version, platform, data schema, and required feature set.
- Let each product pin an exact core release. Upgrade one staged channel first,
  retain the previous artifact for rollback, and separate binary rollback from
  database rollback.
- Use distinct data directories for different products, user profiles, test
  environments, and concurrently running core instances.
- Put reusable capabilities in this repository. Keep UI copy, product branding,
  packaging policy, and product-only presentation in the consuming product.
- When a product needs a breaking contract change, add negotiated capability or
  version support first, migrate all supported consumers, and remove the legacy
  path only after the documented support window.

## Attribution and licensing review

The root [LICENSE](LICENSE) carries the Apache License 2.0 terms and the
upstream copyright notice. Do not delete or replace it during a sync. Files
modified for CentaurAI distribution must carry any notices required by the
license, and applicable upstream attribution notices must be retained.

For every intake:

- Check whether upstream added or changed `LICENSE`, `NOTICE`, dependency
  licenses, vendored code, fonts, models, prompts, or other distributable
  assets.
- Include a readable copy of required `NOTICE` attributions if upstream begins
  distributing one.
- Record material CentaurAI modifications in release notes or file notices as
  appropriate.
- Treat upstream names and logos as third-party marks. Use **AionCore** and
  **AionUi** only for factual origin, compatibility, and attribution statements;
  do not imply that a CentaurAI build is an official or endorsed upstream
  release.

License headers and repository metadata must agree with the distribution's
approved licensing position. Escalate inconsistencies for legal/maintainer
review instead of silently rewriting inherited notices.
