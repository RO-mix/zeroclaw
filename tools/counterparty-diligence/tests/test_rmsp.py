"""RMSP (SME registry) parser and client tests — no network."""

import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from sources.rmsp import (  # noqa: E402
    RmspClient, parse_search_result, find_by_inn, msp_facts, CATEGORY_LABELS,
)

FIXTURES = Path(__file__).parent / "fixtures"


def _config():
    return {"throttling": {"rmsp_min_interval_seconds": 0,
                           "http_timeout_seconds": 5}}


class _Response:
    def __init__(self, payload, status=200):
        self.status_code = status
        self.content = json.dumps(payload, ensure_ascii=False).encode("utf-8")

    def raise_for_status(self):
        if self.status_code >= 400:
            raise RuntimeError(f"HTTP {self.status_code}")


class _Session:
    def __init__(self, payload):
        self._payload = payload
        self.last_params = None

    def get(self, url, params=None, timeout=None):
        self.last_params = params
        return _Response(self._payload)


def _entries():
    payload = json.loads((FIXTURES / "rmsp_search_result.json").read_text("utf-8"))
    return parse_search_result(payload)


def test_parse_search_result_maps_category_and_dates():
    entries = _entries()
    assert len(entries) == 1
    (entry,) = entries
    assert entry["inn"] == "7707001234"
    assert entry["category"] == 1
    assert entry["category_label"] == CATEGORY_LABELS[1] == "Микропредприятие"
    assert entry["inclusion_date"] == "10.10.2022"  # time part stripped
    assert entry["active"] is True
    assert entry["entity_kind"] == "UL"


def test_parse_handles_missing_category():
    entries = parse_search_result({"data": [{"inn": "1", "category": None}]})
    assert entries[0]["category"] is None
    assert entries[0]["category_label"] is None


def test_find_by_inn_exact_match_only():
    entries = _entries()
    assert find_by_inn(entries, "7707001234")["ogrn"] == "1224700016750"
    assert find_by_inn(entries, "770700123") is None  # substring is not a match


def test_msp_facts_in_registry_contract():
    entry = find_by_inn(_entries(), "7707001234")
    facts = msp_facts(entry, True, retrieved_at="2026-10-18T00:00:00+00:00")
    by_field = {f.field: f for f in facts}
    assert by_field["msp.in_registry"].value is True
    assert by_field["msp.category"].value == "Микропредприятие"
    assert by_field["msp.inclusion_date"].value == "10.10.2022"
    assert by_field["msp.active"].value is True
    assert all(f.source == "rmsp" for f in facts)


def test_msp_facts_not_in_registry_is_confirmed_negative():
    (fact,) = msp_facts(None, False)
    assert fact.field == "msp.in_registry"
    assert fact.value is False
    assert fact.confidence == "confirmed"


def test_client_searches_by_query_param():
    payload = json.loads((FIXTURES / "rmsp_search_result.json").read_text("utf-8"))
    session = _Session(payload)
    client = RmspClient(_config(), session=session)
    entry, found = client.lookup_inn("7707001234")
    assert found and entry["inn"] == "7707001234"
    assert session.last_params["query"] == "7707001234"


def test_client_facts_roundtrip():
    payload = json.loads((FIXTURES / "rmsp_search_result.json").read_text("utf-8"))
    client = RmspClient(_config(), session=_Session(payload))
    facts = client.facts_for_inn("7707001234")
    assert any(f.field == "msp.in_registry" and f.value is True for f in facts)
