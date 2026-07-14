#!/usr/bin/env python3
"""Audit a CentaurAI Core database migration against isolated data copies."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import signal
import sqlite3
import subprocess
import sys
import threading
import time
from typing import Any
import urllib.error
import urllib.request


DATABASE_NAMES = ("aionui-backend.db", "aionui.db")
LISTENING_PREFIX = "AIONCORE_LISTENING "
CANONICAL_SERVICE = "centaurai-core"
API_PATHS = (
    "/api/settings",
    "/api/providers",
    "/api/conversations",
    "/api/assistants",
    "/api/teams",
    "/api/mcp/servers",
    "/api/skills",
)
CRITICAL_TABLES = (
    "users",
    "system_settings",
    "client_preferences",
    "providers",
    "conversations",
    "messages",
    "assistants",
    "assistant_definitions",
    "assistant_overlays",
    "assistant_preferences",
    "teams",
    "mailbox",
    "team_tasks",
    "cron_jobs",
    "mcp_servers",
    "remote_agents",
    "skills",
    "acp_session",
)
CUSTOM_REFERENCES = (
    ("messages", "conversation_id", "conversations", "id"),
    ("conversations", "user_id", "users", "id"),
    ("teams", "user_id", "users", "id"),
    ("cron_jobs", "conversation_id", "conversations", "id"),
    ("assistant_sessions", "user_id", "assistant_users", "id"),
    ("assistant_sessions", "conversation_id", "conversations", "id"),
    ("mailbox", "team_id", "teams", "id"),
    ("team_tasks", "team_id", "teams", "id"),
    ("acp_session", "conversation_id", "conversations", "id"),
)
CREDENTIAL_COLUMNS = (
    ("providers", "api_key_encrypted"),
    ("providers", "api_key"),
    ("oauth_tokens", "access_token"),
    ("oauth_tokens", "refresh_token"),
    ("remote_agents", "auth_token"),
    ("remote_agents", "device_token"),
)
WORKSPACE_COLUMNS = (
    ("teams", "workspace"),
    ("assistant_sessions", "workspace"),
)
EXPECTED_REMOVED_KEYS = {
    "client_preferences": {
        "acp.config",
        "aionrs.config",
        "codex.config",
        "acp.cachedModes",
        "acp.cachedInitializeResult",
        "acp.cached_config_options",
    }
}
INVENTORY_DIRS = (
    "assistant-rules",
    "skills",
    "conversations",
    "workspaces",
    "shared-drive",
)


class MigrationAuditError(RuntimeError):
    """Raised when a migration safety or acceptance check fails."""


def _quote(identifier: str) -> str:
    return '"' + identifier.replace('"', '""') + '"'


def _sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file_handle:
        for chunk in iter(lambda: file_handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _is_relative_to(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


def validate_paths(source_data_dir: Path, output_dir: Path, core_bin: Path) -> None:
    source = source_data_dir.resolve()
    output = output_dir.resolve()
    binary = core_bin.resolve()
    if not source.is_dir():
        raise MigrationAuditError(f"source data directory does not exist: {source}")
    if output.exists():
        raise MigrationAuditError(f"output directory must not already exist: {output}")
    if _is_relative_to(output, source):
        raise MigrationAuditError("output directory must not be inside the source data directory")
    if not binary.is_file():
        raise MigrationAuditError(f"core binary does not exist: {binary}")
    if os.name != "nt" and not os.access(binary, os.X_OK):
        raise MigrationAuditError(f"core binary is not executable: {binary}")


def locate_database(data_dir: Path) -> Path:
    for name in DATABASE_NAMES:
        candidate = data_dir / name
        if candidate.is_file():
            return candidate
    raise MigrationAuditError(f"no supported SQLite database found under {data_dir}")


def _sqlite_backup(source: Path, destination: Path, immutable_source: bool = False) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.unlink(missing_ok=True)
    for suffix in ("-wal", "-shm"):
        Path(str(destination) + suffix).unlink(missing_ok=True)
    source_uri = f"{source.resolve().as_uri()}?mode=ro"
    if immutable_source:
        source_uri += "&immutable=1"
    with sqlite3.connect(source_uri, uri=True, timeout=30) as source_db:
        with sqlite3.connect(destination, timeout=30) as destination_db:
            source_db.backup(destination_db)


def create_consistent_copy(source_data_dir: Path, destination: Path) -> None:
    """Copy a data directory and replace SQLite files with online backups."""
    shutil.copytree(source_data_dir, destination, symlinks=True)
    for name in DATABASE_NAMES:
        source_db = source_data_dir / name
        if source_db.is_file():
            _sqlite_backup(source_db, destination / name)


def clone_snapshot(source_snapshot: Path, destination: Path) -> None:
    shutil.copytree(source_snapshot, destination, symlinks=True)
    for name in DATABASE_NAMES:
        source_db = source_snapshot / name
        if source_db.is_file():
            _sqlite_backup(source_db, destination / name, immutable_source=True)


def tree_fingerprint(root: Path) -> dict[str, Any]:
    digest = hashlib.sha256()
    files = 0
    total_bytes = 0
    for path in sorted(root.rglob("*"), key=lambda item: item.relative_to(root).as_posix()):
        relative = path.relative_to(root).as_posix()
        if path.is_symlink():
            digest.update(f"L\0{relative}\0{os.readlink(path)}\n".encode())
            continue
        if not path.is_file():
            continue
        files += 1
        size = path.stat().st_size
        total_bytes += size
        digest.update(f"F\0{relative}\0{size}\0".encode())
        digest.update(_sha256_file(path).encode())
        digest.update(b"\n")
    return {"sha256": digest.hexdigest(), "fileCount": files, "totalBytes": total_bytes}


def _table_columns(connection: sqlite3.Connection, table: str) -> list[dict[str, Any]]:
    rows = connection.execute(f"PRAGMA table_info({_quote(table)})").fetchall()
    return [
        {"name": row[1], "type": row[2], "notNull": bool(row[3]), "default": row[4], "primaryKey": row[5]}
        for row in rows
    ]


def _table_key_columns(columns: list[dict[str, Any]]) -> list[str]:
    primary = sorted((column for column in columns if column["primaryKey"]), key=lambda column: column["primaryKey"])
    if primary:
        return [column["name"] for column in primary]
    for candidate in ("id", "key", "conversation_id", "server_url", "version"):
        if any(column["name"] == candidate for column in columns):
            return [candidate]
    return []


def _key_tokens(connection: sqlite3.Connection, table: str, key_columns: list[str]) -> set[str]:
    if not key_columns:
        return set()
    projection = ", ".join(_quote(column) for column in key_columns)
    rows = connection.execute(f"SELECT {projection} FROM {_quote(table)}").fetchall()
    return {json.dumps(list(row), ensure_ascii=True, separators=(",", ":"), default=str) for row in rows}


def _reference_audit(
    connection: sqlite3.Connection,
    tables: set[str],
    columns_by_table: dict[str, set[str]],
) -> dict[str, int]:
    results: dict[str, int] = {}
    for child, child_column, parent, parent_column in CUSTOM_REFERENCES:
        if child not in tables or parent not in tables:
            continue
        if child_column not in columns_by_table[child] or parent_column not in columns_by_table[parent]:
            continue
        sql = (
            f"SELECT COUNT(*) FROM {_quote(child)} child "
            f"LEFT JOIN {_quote(parent)} parent ON child.{_quote(child_column)} = parent.{_quote(parent_column)} "
            f"WHERE child.{_quote(child_column)} IS NOT NULL "
            f"AND TRIM(CAST(child.{_quote(child_column)} AS TEXT)) <> '' "
            f"AND parent.{_quote(parent_column)} IS NULL"
        )
        results[f"{child}.{child_column}->{parent}.{parent_column}"] = int(connection.execute(sql).fetchone()[0])
    return results


def _nonempty_column_counts(
    connection: sqlite3.Connection,
    tables: set[str],
    columns_by_table: dict[str, set[str]],
    specs: tuple[tuple[str, str], ...],
) -> dict[str, int]:
    counts: dict[str, int] = {}
    for table, column in specs:
        if table not in tables or column not in columns_by_table[table]:
            continue
        sql = (
            f"SELECT COUNT(*) FROM {_quote(table)} WHERE {_quote(column)} IS NOT NULL "
            f"AND TRIM(CAST({_quote(column)} AS TEXT)) <> ''"
        )
        counts[f"{table}.{column}"] = int(connection.execute(sql).fetchone()[0])
    return counts


def _conversation_workspace_counts(connection: sqlite3.Connection, tables: set[str]) -> dict[str, int]:
    if "conversations" not in tables:
        return {}
    columns = {column["name"] for column in _table_columns(connection, "conversations")}
    if "extra" not in columns:
        return {}
    total = 0
    parse_errors = 0
    for (raw_extra,) in connection.execute("SELECT extra FROM conversations WHERE extra IS NOT NULL"):
        try:
            extra = json.loads(raw_extra) if isinstance(raw_extra, str) else raw_extra
        except (TypeError, ValueError):
            parse_errors += 1
            continue
        if isinstance(extra, dict) and isinstance(extra.get("workspace"), str) and extra["workspace"].strip():
            total += 1
    return {"conversations.extra.workspace": total, "conversations.extra.parseErrors": parse_errors}


def _directory_inventory(data_dir: Path) -> dict[str, dict[str, int]]:
    inventory: dict[str, dict[str, int]] = {}
    for relative in INVENTORY_DIRS:
        root = data_dir / relative
        if not root.is_dir():
            continue
        file_count = 0
        total_bytes = 0
        for path in root.rglob("*"):
            if path.is_file() and not path.is_symlink():
                file_count += 1
                total_bytes += path.stat().st_size
        inventory[relative] = {"fileCount": file_count, "totalBytes": total_bytes}
    return inventory


def audit_database(data_dir: Path, immutable: bool = False) -> tuple[dict[str, Any], dict[str, set[str]]]:
    database_path = locate_database(data_dir)
    database_uri = f"{database_path.resolve().as_uri()}?mode=ro"
    if immutable:
        database_uri += "&immutable=1"
    with sqlite3.connect(database_uri, uri=True, timeout=30) as connection:
        tables = {
            row[0]
            for row in connection.execute(
                "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
            )
        }
        schema_rows = connection.execute(
            "SELECT type, name, tbl_name, COALESCE(sql, '') FROM sqlite_schema "
            "WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name"
        ).fetchall()
        schema_text = json.dumps(schema_rows, ensure_ascii=True, separators=(",", ":"))
        row_counts: dict[str, int] = {}
        key_summary: dict[str, dict[str, Any]] = {}
        keys: dict[str, set[str]] = {}
        columns_by_table: dict[str, set[str]] = {}
        for table in sorted(tables):
            columns = _table_columns(connection, table)
            columns_by_table[table] = {column["name"] for column in columns}
            row_counts[table] = int(connection.execute(f"SELECT COUNT(*) FROM {_quote(table)}").fetchone()[0])
            key_columns = _table_key_columns(columns)
            table_keys = _key_tokens(connection, table, key_columns)
            keys[table] = table_keys
            key_summary[table] = {
                "columns": key_columns,
                "count": len(table_keys),
                "sha256": _sha256_bytes("\n".join(sorted(table_keys)).encode()),
            }

        quick_check_rows = [row[0] for row in connection.execute("PRAGMA quick_check").fetchall()]
        foreign_key_violations = len(connection.execute("PRAGMA foreign_key_check").fetchall())
        migrations: dict[str, Any] = {"count": 0, "maxVersion": None}
        if "_sqlx_migrations" in tables:
            migration_row = connection.execute("SELECT COUNT(*), MAX(version) FROM _sqlx_migrations").fetchone()
            migrations = {"count": int(migration_row[0]), "maxVersion": migration_row[1]}

        audit = {
            "databaseName": database_path.name,
            "databaseSha256": _sha256_file(database_path),
            "quickCheck": "ok" if quick_check_rows == ["ok"] else "failed",
            "quickCheckResultCount": len(quick_check_rows),
            "foreignKeyViolationCount": foreign_key_violations,
            "schemaSha256": _sha256_bytes(schema_text.encode()),
            "schemaObjectCount": len(schema_rows),
            "tables": sorted(tables),
            "rowCounts": row_counts,
            "keySets": key_summary,
            "migrations": migrations,
            "customOrphanCounts": _reference_audit(connection, tables, columns_by_table),
            "credentialReferenceCounts": _nonempty_column_counts(
                connection, tables, columns_by_table, CREDENTIAL_COLUMNS
            ),
            "workspaceReferenceCounts": {
                **_nonempty_column_counts(connection, tables, columns_by_table, WORKSPACE_COLUMNS),
                **_conversation_workspace_counts(connection, tables),
            },
            "dataInventory": _directory_inventory(data_dir),
        }
        return audit, keys


def compare_database_audits(
    before: dict[str, Any],
    before_keys: dict[str, set[str]],
    after: dict[str, Any],
    after_keys: dict[str, set[str]],
) -> dict[str, Any]:
    checks: list[dict[str, Any]] = []

    def add(name: str, passed: bool, **details: Any) -> None:
        checks.append({"name": name, "passed": passed, **details})

    add("after.quick_check", after["quickCheck"] == "ok", result=after["quickCheck"])
    add(
        "after.foreign_keys",
        after["foreignKeyViolationCount"] == 0,
        violationCount=after["foreignKeyViolationCount"],
    )
    orphan_total = sum(after["customOrphanCounts"].values())
    add("after.custom_references", orphan_total == 0, orphanCount=orphan_total)

    for table in CRITICAL_TABLES:
        if table not in before["rowCounts"]:
            continue
        before_count = before["rowCounts"][table]
        after_count = after["rowCounts"].get(table)
        missing_keys = before_keys.get(table, set()) - after_keys.get(table, set())
        expected_removed_tokens = {
            json.dumps([key], ensure_ascii=True, separators=(",", ":"))
            for key in EXPECTED_REMOVED_KEYS.get(table, set())
        }
        unexpected_missing_keys = missing_keys - expected_removed_tokens
        add(
            f"table.{table}.preserved",
            after_count is not None
            and after_count >= before_count - len(missing_keys & expected_removed_tokens)
            and not unexpected_missing_keys,
            beforeCount=before_count,
            afterCount=after_count,
            missingKeyCount=len(unexpected_missing_keys),
            expectedRemovedKeyCount=len(missing_keys & expected_removed_tokens),
            addedKeyCount=len(after_keys.get(table, set()) - before_keys.get(table, set())),
        )

    for name, before_count in before["credentialReferenceCounts"].items():
        after_count = after["credentialReferenceCounts"].get(name, 0)
        add(
            f"credential_reference.{name}",
            after_count >= before_count,
            beforeCount=before_count,
            afterCount=after_count,
        )

    return {"passed": all(check["passed"] for check in checks), "checks": checks}


def _summarize_json(body: Any) -> dict[str, Any]:
    summary: dict[str, Any] = {"jsonType": type(body).__name__}
    data = body.get("data") if isinstance(body, dict) and "data" in body else body
    if isinstance(body, dict) and isinstance(body.get("success"), bool):
        summary["success"] = body["success"]
    if isinstance(data, list):
        summary["itemCount"] = len(data)
    elif isinstance(data, dict):
        summary["fieldCount"] = len(data)
        for collection_key in ("items", "conversations", "teams", "assistants", "providers", "servers", "skills"):
            collection = data.get(collection_key)
            if isinstance(collection, list):
                summary["itemCount"] = len(collection)
                break
    return summary


def _get_json(url: str, timeout: float = 5.0) -> tuple[int, Any]:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:
            return response.status, json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as error:
        return error.code, None


def _wait_for_health(port: int, timeout_seconds: float) -> tuple[int, Any]:
    deadline = time.monotonic() + timeout_seconds
    last_status: tuple[int, Any] | None = None
    while time.monotonic() < deadline:
        try:
            last_status = _get_json(f"http://127.0.0.1:{port}/health", timeout=1.5)
            if last_status[0] == 200:
                return last_status
        except (OSError, ValueError, urllib.error.URLError):
            pass
        time.sleep(0.2)
    raise MigrationAuditError(f"core health check timed out on dynamic port {port}; last status: {last_status}")


def parse_listening_event(line: str) -> int | None:
    if not line.startswith(LISTENING_PREFIX):
        return None
    try:
        payload = json.loads(line[len(LISTENING_PREFIX) :])
        port = int(payload["port"])
    except (KeyError, TypeError, ValueError, json.JSONDecodeError):
        return None
    return port if 0 < port <= 65535 else None


def _stream_output(process: subprocess.Popen[str], log_path: Path, lines: queue.Queue[str]) -> None:
    assert process.stdout is not None
    with log_path.open("w", encoding="utf-8") as log_file:
        for line in process.stdout:
            log_file.write(line)
            log_file.flush()
            lines.put(line.rstrip("\r\n"))


def _stop_process(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    if os.name == "nt":
        process.terminate()
    else:
        process.send_signal(signal.SIGTERM)
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def run_core_smoke(
    binary: Path,
    data_dir: Path,
    log_dir: Path,
    startup_log: Path,
    timeout_seconds: float,
    expected_version: str | None,
    expected_commit: str | None,
    canonical: bool,
    managed_resources_dir: Path | None = None,
) -> dict[str, Any]:
    log_dir.mkdir(parents=True, exist_ok=True)
    command = [
        str(binary.resolve()),
        "--host",
        "127.0.0.1",
        "--port",
        "0",
        "--data-dir",
        str(data_dir.resolve()),
        "--log-dir",
        str(log_dir.resolve()),
        "--local",
    ]
    environment = os.environ.copy()
    if managed_resources_dir is not None:
        command.extend(["--managed-resources-mode", "bundled"])
        environment["AIONUI_BUNDLED_MANAGED_RESOURCES"] = str(managed_resources_dir.resolve())

    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
        env=environment,
    )
    output_lines: queue.Queue[str] = queue.Queue()
    pump = threading.Thread(target=_stream_output, args=(process, startup_log, output_lines), daemon=True)
    pump.start()
    port: int | None = None
    deadline = time.monotonic() + timeout_seconds
    try:
        while time.monotonic() < deadline and port is None:
            if process.poll() is not None:
                raise MigrationAuditError(
                    f"core exited before announcing a listening port (exit {process.returncode}); see {startup_log}"
                )
            try:
                port = parse_listening_event(output_lines.get(timeout=0.2))
            except queue.Empty:
                continue
        if port is None:
            raise MigrationAuditError(f"core did not announce a listening port; see {startup_log}")

        status, health = _wait_for_health(port, max(1.0, deadline - time.monotonic()))
        if not isinstance(health, dict):
            raise MigrationAuditError("core health response is not a JSON object")
        checks: list[dict[str, Any]] = [{"name": "health.http", "passed": status == 200, "status": status}]
        if canonical:
            checks.extend(
                [
                    {
                        "name": "health.service",
                        "passed": health.get("service") == CANONICAL_SERVICE,
                        "actual": health.get("service"),
                    },
                    {
                        "name": "health.version",
                        "passed": expected_version is None or health.get("version") == expected_version,
                        "actual": health.get("version"),
                        "expected": expected_version,
                    },
                    {
                        "name": "health.commit",
                        "passed": bool(health.get("commit"))
                        and health.get("commit") != "unknown"
                        and (expected_commit is None or health.get("commit") == expected_commit),
                        "actual": health.get("commit"),
                        "expected": expected_commit,
                    },
                ]
            )
        elif expected_version is not None:
            checks.append(
                {
                    "name": "health.legacy_version",
                    "passed": health.get("version") == expected_version,
                    "actual": health.get("version"),
                    "expected": expected_version,
                }
            )

        api_results: dict[str, Any] = {}
        for path in API_PATHS:
            api_status, body = _get_json(f"http://127.0.0.1:{port}{path}")
            passed = 200 <= api_status < 300
            checks.append({"name": f"api.{path}", "passed": passed, "status": api_status})
            api_results[path] = {"status": api_status, **_summarize_json(body)}

        return {
            "passed": all(check["passed"] for check in checks),
            "port": port,
            "health": {
                key: health.get(key) for key in ("status", "service", "version", "commit", "build_time")
            },
            "checks": checks,
            "api": api_results,
            "startupLog": str(startup_log),
        }
    finally:
        _stop_process(process)
        pump.join(timeout=2)


def binary_provenance(binary: Path) -> dict[str, Any]:
    completed = subprocess.run(
        [str(binary.resolve()), "--version"], capture_output=True, text=True, timeout=10, check=False
    )
    version_output = (completed.stdout or completed.stderr).strip().splitlines()
    manifest_path = binary.parent / "manifest.json"
    manifest: dict[str, Any] | None = None
    if manifest_path.is_file():
        try:
            raw_manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            allowed = (
                "repository",
                "tag",
                "commit",
                "artifactUrl",
                "sha256",
                "archiveSha256",
                "binaryName",
                "sourceBinaryName",
                "fallbackUsed",
            )
            manifest = {key: raw_manifest.get(key) for key in allowed if key in raw_manifest}
        except (OSError, ValueError):
            manifest = {"invalid": True}
    return {
        "path": str(binary.resolve()),
        "sha256": _sha256_file(binary),
        "versionOutput": version_output[0] if version_output else None,
        "versionExitCode": completed.returncode,
        "manifest": manifest,
    }


def _write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=True, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def execute(args: argparse.Namespace) -> dict[str, Any]:
    source = args.source_data_dir.resolve()
    output = args.output_dir.resolve()
    core_bin = args.core_bin.resolve()
    validate_paths(source, output, core_bin)
    if args.legacy_core_bin is not None:
        legacy = args.legacy_core_bin.resolve()
        if not legacy.is_file():
            raise MigrationAuditError(f"legacy core binary does not exist: {legacy}")

    output.mkdir(parents=True)
    rollback_data = output / "rollback-data"
    upgrade_data = output / "upgrade-data"
    reports_dir = output / "reports"
    logs_dir = output / "logs"

    create_consistent_copy(source, rollback_data)
    before, before_keys = audit_database(rollback_data, immutable=True)
    rollback_before = tree_fingerprint(rollback_data)
    _write_json(reports_dir / "before.json", before)

    clone_snapshot(rollback_data, upgrade_data)
    core_provenance = binary_provenance(core_bin)
    smoke = run_core_smoke(
        core_bin,
        upgrade_data,
        logs_dir / "upgrade-core",
        logs_dir / "upgrade-core.stdout.log",
        args.timeout,
        args.expected_version,
        args.expected_commit,
        canonical=True,
        managed_resources_dir=args.managed_resources_dir,
    )
    after, after_keys = audit_database(upgrade_data)
    _write_json(reports_dir / "after.json", after)
    comparison = compare_database_audits(before, before_keys, after, after_keys)
    comparison["coreSmokePassed"] = smoke["passed"]
    comparison["passed"] = comparison["passed"] and smoke["passed"]
    _write_json(reports_dir / "comparison.json", comparison)

    rollback_after = tree_fingerprint(rollback_data)
    rollback_immutable = rollback_before == rollback_after
    rollback_drill: dict[str, Any] = {"status": "not-run", "passed": None}
    if args.legacy_core_bin is not None:
        rollback_drill_data = output / "rollback-drill-data"
        clone_snapshot(rollback_data, rollback_drill_data)
        legacy_provenance = binary_provenance(args.legacy_core_bin)
        legacy_smoke = run_core_smoke(
            args.legacy_core_bin,
            rollback_drill_data,
            logs_dir / "rollback-core",
            logs_dir / "rollback-core.stdout.log",
            args.timeout,
            args.legacy_expected_version,
            None,
            canonical=False,
        )
        rollback_drill = {
            "status": "passed" if legacy_smoke["passed"] else "failed",
            "passed": legacy_smoke["passed"],
            "core": legacy_provenance,
            "smoke": legacy_smoke,
            "dataDirectory": str(rollback_drill_data),
        }

    passed = comparison["passed"] and rollback_immutable and rollback_drill.get("passed") is not False
    return {
        "status": "passed" if passed else "failed",
        "passed": passed,
        "sourceDataDirectory": str(source),
        "outputDirectory": str(output),
        "core": core_provenance,
        "beforeReport": str(reports_dir / "before.json"),
        "afterReport": str(reports_dir / "after.json"),
        "comparisonReport": str(reports_dir / "comparison.json"),
        "upgradeSmoke": smoke,
        "rollbackSnapshot": {
            "dataDirectory": str(rollback_data),
            "before": rollback_before,
            "after": rollback_after,
            "immutable": rollback_immutable,
        },
        "rollbackDrill": rollback_drill,
        "rollbackRule": (
            "Never start a legacy core against upgrade-data. Stop the new core and restore rollback-data "
            "to a fresh production path before starting the legacy core."
        ),
    }


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Audit v0.1.46 against a consistent data-directory copy and preserve rollback data."
    )
    parser.add_argument("--source-data-dir", type=Path, required=True)
    parser.add_argument("--core-bin", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--expected-version", default="0.1.46")
    parser.add_argument("--expected-commit")
    parser.add_argument("--managed-resources-dir", type=Path)
    parser.add_argument("--legacy-core-bin", type=Path)
    parser.add_argument("--legacy-expected-version", default="0.1.24")
    parser.add_argument("--timeout", type=float, default=90.0)
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    report_path = args.output_dir.resolve() / "migration-report.json"
    try:
        report = execute(args)
    except Exception as error:
        report = {
            "status": "failed",
            "passed": False,
            "error": str(error),
            "rollbackRule": (
                "Do not start a legacy core against any upgraded database. Restore an upgrade-before snapshot first."
            ),
        }
        if args.output_dir.exists() and args.output_dir.is_dir():
            _write_json(report_path, report)
        print(f"migration audit failed: {error}", file=sys.stderr)
        return 1

    _write_json(report_path, report)
    print(json.dumps({"status": report["status"], "report": str(report_path)}, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
