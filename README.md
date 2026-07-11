# CentaurAI Core

CentaurAI Core is the shared agent runtime and backend service for CentaurAI
products. It owns the HTTP and WebSocket API, persistence, authentication,
agent-process lifecycle, provider integration, MCP/ACP integration, scheduled
work, and team orchestration used by desktop, web, and other CentaurAI hosts.

This repository began as a fork of
[AionCore](https://github.com/iOfficeAI/AionCore). It is now maintained and
released independently by the CentaurAI team. Upstream changes are reviewed and
integrated deliberately; a CentaurAI release is not an official AionCore or
AionUi release and does not imply endorsement by those projects.

The canonical product name is **CentaurAI Core**, and the service executable is
`centaurai-core`. Some crate names, database paths, and startup messages still
use `aioncore` or `aionui`. Those identifiers are retained where changing them
would break existing hosts; they are compatibility surfaces, not the CentaurAI
brand.

## Integration contract

Products should consume CentaurAI Core as a versioned service executable, not
as an unstable Rust library API. The supported host boundary is:

- REST APIs under `/api/*`, with `/health` for readiness checks.
- Real-time events through the `/ws` WebSocket endpoint.
- A product-specific `--data-dir`, containing the SQLite database and runtime
  state. The established database filename is `aionui-backend.db`.
- The legacy stdout listening record
  `AIONCORE_LISTENING {"host":"127.0.0.1","port":25808}`. Hosts must parse
  the JSON payload rather than assume a fixed port.

Existing AionCore-compatible integrations are a deliberate compatibility
target. Changes to REST behavior, WebSocket event names or payloads, the startup
record, or SQLite data migration require an explicit compatibility review.
Additive evolution is preferred; breaking changes require a versioned migration
and a coordinated product rollout.

SQLite is a persistence format, not a general-purpose public API. Existing
CentaurAI bridges that read particular tables are known consumers and must be
included in schema impact reviews. Do not run two core processes against the
same live data directory. Forward migrations are supported; downgrading a
migrated database is not assumed to be safe.

## Using one core across products

Each product pins a tested CentaurAI Core release and packages or deploys its
artifact through that product's normal release pipeline. For an embedded host:

```text
product host
  -> start centaurai-core with --host 127.0.0.1 --port 0
       --data-dir <product-owned-dir>
  -> read AIONCORE_LISTENING from stdout
  -> verify /health
  -> use /api/* and /ws
```

Use a separate data directory for every product profile and environment. A
single intentionally shared server may serve more than one client, but those
clients then share that server's identity, authorization, lifecycle, and data;
that deployment must be designed and tested as shared infrastructure.

Product-specific behavior should enter the core through reusable capabilities,
configuration, or negotiated feature support. Avoid product forks and avoid
making a product track this repository's `main` branch directly. Promote a core
release only after its API, migration, and runtime behavior pass that product's
compatibility suite.

## Development

The workspace is written in Rust and uses Axum, Tokio, and SQLite. See
[ARCHITECTURE.md](ARCHITECTURE.md) for the crate structure and [AGENTS.md](AGENTS.md)
for repository rules. Common development commands are defined in the
[`justfile`](justfile).

Upstream remote setup, intake policy, and the release checklist are documented
in [UPSTREAM.md](UPSTREAM.md).

## License and names

This repository contains work derived from AionCore. The inherited upstream
work is provided under the Apache License 2.0; see [LICENSE](LICENSE) and
[NOTICE](NOTICE). Preserve applicable copyright, attribution, license, and
modification notices when redistributing derivative work, and review any
additional `NOTICE` content introduced by a future upstream sync.

Apache-2.0 does not grant rights to upstream trade names, trademarks, service
marks, or product names except for customary attribution and description of
origin. The names **AionCore** and **AionUi** are used here only to identify the
upstream project and compatibility history. **CentaurAI Core** is independently
maintained and should use CentaurAI-owned branding in product-facing material.
