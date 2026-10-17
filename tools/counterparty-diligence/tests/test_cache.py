from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from cache import Cache  # noqa: E402
from sources.base import Fact  # noqa: E402

FRESH = "2026-10-17T00:00:00+00:00"
STALE = "2026-10-15T00:00:00+00:00"


def test_facts_roundtrip(tmp_path):
    cache = Cache(tmp_path / "t.sqlite")
    facts = [Fact(field="inn", value="7707001234", source="egrul",
                  retrieved_at=FRESH)]
    cache.set_facts("7707001234", facts)
    got = cache.get_facts("7707001234", ttl_hours=24)
    assert got and got[0].value == "7707001234"
    assert got[0].field == "inn" and got[0].source == "egrul"
    cache.close()


def test_facts_missing_returns_none(tmp_path):
    cache = Cache(tmp_path / "t.sqlite")
    assert cache.get_facts("0000000000") is None
    cache.close()


def test_facts_expired_returns_none(tmp_path):
    cache = Cache(tmp_path / "t.sqlite")
    cache.set_facts("7707001234",
                    [Fact(field="inn", value="x", source="egrul",
                          retrieved_at=STALE)])
    assert cache.get_facts("7707001234", ttl_hours=24) is None
    cache.close()


def test_list_version_roundtrip(tmp_path):
    cache = Cache(tmp_path / "t.sqlite")
    cache.set_list_version("ofac_sdn", "2026-10-16")
    info = cache.get_list_version("ofac_sdn")
    assert info["version_date"] == "2026-10-16"
    assert info["downloaded_at"]
    cache.close()


def test_replace_sanctions_list_swaps_atomically(tmp_path):
    cache = Cache(tmp_path / "t.sqlite")
    cache.replace_sanctions_list(
        "ofac_sdn", [{"alias": "IVAN IVANOV",
                      "normalized_name": "ivan ivanov",
                      "raw": {"a": 1}}], "2026-10-16")
    cache.replace_sanctions_list(
        "ofac_sdn", [{"alias": "PETR PETROV",
                      "normalized_name": "petr petrov",
                      "raw": {"a": 2}}], "2026-10-17")
    entries = cache.all_sanctions_entries()
    assert len(entries) == 1
    assert entries[0]["alias"] == "PETR PETROV"
    assert entries[0]["version_date"] == "2026-10-17"
    assert cache.get_list_version("ofac_sdn")["version_date"] == "2026-10-17"
    cache.close()
