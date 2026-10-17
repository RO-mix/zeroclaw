"""SQLite cache: facts per INN (TTL 24h) + sanctions list versions/entries."""

from __future__ import annotations

import json
import sqlite3
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Iterable, Optional

from sources.base import Fact

DEFAULT_DB_PATH = Path(__file__).parent / "data" / "dd-cache.sqlite"

_SCHEMA = """
CREATE TABLE IF NOT EXISTS facts (
    inn TEXT NOT NULL,
    field TEXT NOT NULL,
    source TEXT NOT NULL,
    value TEXT,
    source_url TEXT,
    retrieved_at TEXT,
    confidence TEXT,
    PRIMARY KEY (inn, field, source)
);
CREATE TABLE IF NOT EXISTS list_versions (
    list_name TEXT PRIMARY KEY,
    version_date TEXT,
    downloaded_at TEXT
);
CREATE TABLE IF NOT EXISTS sanctions_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    list_name TEXT NOT NULL,
    alias TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    raw_json TEXT,
    version_date TEXT
);
CREATE INDEX IF NOT EXISTS idx_sanctions_norm ON sanctions_entries(normalized_name);
"""


class Cache:
    def __init__(self, db_path: Optional[Path] = None):
        self.db_path = Path(db_path) if db_path else DEFAULT_DB_PATH
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._conn = sqlite3.connect(str(self.db_path))
        self._conn.executescript(_SCHEMA)

    def close(self) -> None:
        self._conn.close()

    def __enter__(self) -> "Cache":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    # --- facts (PRD FR10: card valid 24h for registry data) ---

    def set_facts(self, inn: str, facts: Iterable[Fact]) -> None:
        for fact in facts:
            self._conn.execute(
                "INSERT OR REPLACE INTO facts "
                "(inn, field, source, value, source_url, retrieved_at, confidence) "
                "VALUES (?, ?, ?, ?, ?, ?, ?)",
                (inn, fact.field, fact.source,
                 json.dumps(fact.value, ensure_ascii=False), fact.source_url,
                 fact.retrieved_at, fact.confidence),
            )
        self._conn.commit()

    def get_facts(self, inn: str, ttl_hours: float = 24.0) -> Optional[list[Fact]]:
        rows = self._conn.execute(
            "SELECT field, source, value, source_url, retrieved_at, confidence "
            "FROM facts WHERE inn = ?", (inn,),
        ).fetchall()
        if not rows:
            return None
        cutoff = datetime.now(timezone.utc) - timedelta(hours=ttl_hours)
        facts = []
        for field, source, value, url, retrieved_at, confidence in rows:
            try:
                ts = datetime.fromisoformat(retrieved_at)
            except (TypeError, ValueError):
                return None  # unparsable timestamp -> treat as stale
            if ts < cutoff:
                return None
            facts.append(Fact(field=field, value=json.loads(value), source=source,
                              source_url=url, retrieved_at=retrieved_at,
                              confidence=confidence))
        return facts

    # --- sanctions lists ---

    def set_list_version(self, list_name: str, version_date: str) -> None:
        self._conn.execute(
            "INSERT OR REPLACE INTO list_versions (list_name, version_date, downloaded_at) "
            "VALUES (?, ?, ?)",
            (list_name, version_date,
             datetime.now(timezone.utc).replace(microsecond=0).isoformat()),
        )
        self._conn.commit()

    def get_list_version(self, list_name: str) -> Optional[dict]:
        row = self._conn.execute(
            "SELECT list_name, version_date, downloaded_at FROM list_versions "
            "WHERE list_name = ?", (list_name,),
        ).fetchone()
        if not row:
            return None
        return {"list_name": row[0], "version_date": row[1],
                "downloaded_at": row[2]}

    def replace_sanctions_list(self, list_name: str, entries: Iterable[dict],
                               version_date: str) -> int:
        """Atomically swap one list: delete old entries, insert new."""
        with self._conn:
            self._conn.execute(
                "DELETE FROM sanctions_entries WHERE list_name = ?", (list_name,))
            for entry in entries:
                self._conn.execute(
                    "INSERT INTO sanctions_entries "
                    "(list_name, alias, normalized_name, raw_json, version_date) "
                    "VALUES (?, ?, ?, ?, ?)",
                    (list_name, entry["alias"], entry["normalized_name"],
                     json.dumps(entry["raw"], ensure_ascii=False), version_date),
                )
            self.set_list_version(list_name, version_date)
        return self._conn.total_changes

    def all_sanctions_entries(self) -> list[dict]:
        rows = self._conn.execute(
            "SELECT list_name, alias, normalized_name, raw_json, version_date "
            "FROM sanctions_entries",
        ).fetchall()
        return [{"list_name": r[0], "alias": r[1], "normalized_name": r[2],
                 "raw": json.loads(r[3]), "version_date": r[4]} for r in rows]
