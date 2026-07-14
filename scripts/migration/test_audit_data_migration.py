#!/usr/bin/env python3
"""Unit tests for audit_data_migration.py."""

import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import audit_data_migration as audit


def create_fixture_database(path: Path, messages: tuple[str, ...] = ("message-1",)) -> None:
    with sqlite3.connect(path) as connection:
        connection.executescript(
            """
            PRAGMA foreign_keys = ON;
            CREATE TABLE users (id TEXT PRIMARY KEY, username TEXT NOT NULL);
            CREATE TABLE conversations (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL REFERENCES users(id),
                extra TEXT NOT NULL DEFAULT '{}'
            );
            CREATE TABLE messages (
                id TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL REFERENCES conversations(id)
            );
            CREATE TABLE providers (id TEXT PRIMARY KEY, api_key_encrypted TEXT NOT NULL);
            INSERT INTO users VALUES ('user-1', 'admin');
            INSERT INTO conversations VALUES ('conversation-1', 'user-1', '{"workspace":"/tmp/work"}');
            INSERT INTO providers VALUES ('provider-1', 'secret-value');
            """
        )
        connection.executemany(
            "INSERT INTO messages VALUES (?, 'conversation-1')", ((message_id,) for message_id in messages)
        )


class MigrationAuditTests(unittest.TestCase):
    def test_consistent_copy_preserves_source_and_ignores_wal_sidecars(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            source = root / "source"
            destination = root / "copy"
            source.mkdir()
            database = source / "aionui.db"
            create_fixture_database(database)
            (source / "aionui.db-wal").write_text("stale", encoding="utf-8")
            original_hash = audit._sha256_file(database)

            audit.create_consistent_copy(source, destination)

            self.assertEqual(audit._sha256_file(database), original_hash)
            self.assertFalse((destination / "aionui.db-wal").exists())
            copied, _ = audit.audit_database(destination)
            self.assertEqual(copied["rowCounts"]["messages"], 1)

    def test_immutable_snapshot_reads_and_clones_do_not_create_sidecars(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            source = root / "source"
            snapshot = root / "snapshot"
            clone = root / "clone"
            source.mkdir()
            create_fixture_database(source / "aionui-backend.db")
            audit.create_consistent_copy(source, snapshot)

            audit.audit_database(snapshot, immutable=True)
            before = audit.tree_fingerprint(snapshot)
            audit.clone_snapshot(snapshot, clone)

            self.assertEqual(audit.tree_fingerprint(snapshot), before)
            self.assertFalse((snapshot / "aionui-backend.db-shm").exists())
            self.assertFalse((snapshot / "aionui-backend.db-wal").exists())

    def test_comparison_reports_missing_keys_without_disclosing_them(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            before_dir = root / "before"
            after_dir = root / "after"
            before_dir.mkdir()
            after_dir.mkdir()
            create_fixture_database(before_dir / "aionui-backend.db", ("message-private-1", "message-private-2"))
            create_fixture_database(after_dir / "aionui-backend.db", ("message-private-1",))

            before, before_keys = audit.audit_database(before_dir)
            after, after_keys = audit.audit_database(after_dir)
            comparison = audit.compare_database_audits(before, before_keys, after, after_keys)
            serialized = json.dumps(comparison)

            self.assertFalse(comparison["passed"])
            self.assertIn('"missingKeyCount": 1', serialized)
            self.assertNotIn("message-private-2", serialized)
            self.assertNotIn("secret-value", json.dumps(before))

    def test_comparison_allows_only_documented_retired_preferences(self):
        before = {
            "quickCheck": "ok",
            "foreignKeyViolationCount": 0,
            "customOrphanCounts": {},
            "rowCounts": {"client_preferences": 2},
            "credentialReferenceCounts": {},
        }
        after = {
            "quickCheck": "ok",
            "foreignKeyViolationCount": 0,
            "customOrphanCounts": {},
            "rowCounts": {"client_preferences": 1},
            "credentialReferenceCounts": {},
        }
        retired = json.dumps(["acp.config"], separators=(",", ":"))
        kept = json.dumps(["theme.activeId"], separators=(",", ":"))

        comparison = audit.compare_database_audits(
            before,
            {"client_preferences": {retired, kept}},
            after,
            {"client_preferences": {kept}},
        )

        self.assertTrue(comparison["passed"])
        preference_check = next(check for check in comparison["checks"] if check["name"].startswith("table."))
        self.assertEqual(preference_check["expectedRemovedKeyCount"], 1)

    def test_validate_paths_rejects_output_inside_source(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            source = Path(temp_dir) / "source"
            source.mkdir()
            binary = Path(temp_dir) / "centaurai-core"
            binary.write_text("fixture", encoding="utf-8")
            binary.chmod(0o755)

            with self.assertRaisesRegex(audit.MigrationAuditError, "must not be inside"):
                audit.validate_paths(source, source / "audit", binary)

    def test_listening_event_parser_rejects_invalid_ports(self):
        self.assertEqual(audit.parse_listening_event('AIONCORE_LISTENING {"host":"127.0.0.1","port":49153}'), 49153)
        self.assertIsNone(audit.parse_listening_event('AIONCORE_LISTENING {"port":0}'))
        self.assertIsNone(audit.parse_listening_event("not-an-event"))


if __name__ == "__main__":
    unittest.main()
