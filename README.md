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
- The public `GET /api/capabilities` negotiation document. Hosts should check
  its REST, WebSocket, startup, and feature versions before enabling optional
  integrations.
- Real-time events through the `/ws` WebSocket endpoint.
- A product-specific `--data-dir`, containing the SQLite database and runtime
  state. The established database filename is `aionui-backend.db`.
- The canonical stdout listening record
  `CENTAURAI_CORE_LISTENING {"host":"127.0.0.1","port":25808}`, followed by
  the legacy-compatible `AIONCORE_LISTENING` record with the same payload.
  Hosts must parse the JSON payload rather than assume a fixed port.

Canonical process configuration uses `CENTAURAI_CORE_*` environment variables.
For each migrated setting, the equivalent legacy `AIONUI_*` name remains
effective; the canonical value wins when both are set. Spawned agent runtimes
receive both namespaces during the compatibility window.

Trusted proxy identity headers use `x-centaurai-proxy-user-id`,
`x-centaurai-proxy-username`, `x-centaurai-proxy-role`,
`x-centaurai-proxy-timestamp`, and `x-centaurai-proxy-signature`. The complete
legacy `x-aionui-proxy-*` family remains accepted. A partial canonical family
fails closed instead of falling back to legacy values.

Browser authentication uses `centaurai-session` and
`centaurai-csrf-token`. During the migration window the legacy
`aionui-session` and `aionui-csrf-token` cookies are also read, written, and
cleared; canonical values take priority when both are present. CSRF validation
applies only when authentication is carried by a session cookie. Native
clients using `Authorization: Bearer ...` still require valid authentication
but do not send a CSRF token.

Context clients pair through `POST /api/devices/pairing` and redeem the
five-minute, one-time code at `POST /api/devices/pairing/redeem`. Pairing URLs
accept only private LAN, mDNS, or Tailscale HTTP(S) server addresses. Device
tokens use the `cai_dev_v1_` prefix, are returned only by the successful redeem
response, and are persisted only as SHA-256 hashes. Revocation immediately
invalidates REST and WebSocket authentication for that credential.

Provider credentials are write-only in create/update requests. Provider
responses return `api_key_mask`, `key_id`, and `api_key_present`; the legacy
`api_key` response field is retained only as a masked compatibility alias.
Clients must omit `api_key` on updates unless the user supplies a new plaintext
secret. Values beginning with `masked:v1:` are response placeholders and are
rejected on write.

Knowledge clients also use Core as their only public boundary. Core exposes
`/api/knowledge/status`, spaces, sources, jobs, search, Wiki, memory, and graph
resources; it authenticates the owner and calls the private
`centaurai-knowledge-worker` with `CENTAURAI_KNOWLEDGE_INTERNAL_TOKEN`. The
preferred transport is the Unix socket configured by
`CENTAURAI_KNOWLEDGE_WORKER_SOCKET` (defaulting under the Core data directory).
`CENTAURAI_KNOWLEDGE_WORKER_URL` is an explicit loopback-only fallback and must
use a literal loopback IP. Client requests cannot override either transport.

An appliance sets `CENTAURAI_CORE_KNOWLEDGE_WORKER_BIN` to an absolute,
executable Worker path. Core then owns that process: it passes the public Core
socket setting to the child as `CENTAURAI_KNOWLEDGE_SOCKET`, passes
`CENTAURAI_KNOWLEDGE_DATA_DIR` and the internal token, waits for Worker
readiness, restarts crashes with capped exponential backoff, and tears the
process tree down during Core shutdown. A configured managed binary fails
closed when its path, token, data directory, or socket is unsafe. Without the
managed-binary setting, Core can still connect to an independently supervised
Worker on its fixed private transport.

Conversation message requests may include a `knowledge` policy with `mode`,
`space_ids`, `max_hits`, and `cloud_use`. Core performs retrieval before model
dispatch, stores the structured `RetrievalBundle` with the user message, and
keeps the visible message text separate from the model-only evidence context.
External or unknown model targets require both per-request authorization and
an `allowed` cloud policy on every selected knowledge space.

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
  -> read CENTAURAI_CORE_LISTENING from stdout
  -> verify /health
  -> negotiate /api/capabilities
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
