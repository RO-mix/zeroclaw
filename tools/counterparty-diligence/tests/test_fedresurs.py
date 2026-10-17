"""Fedresurs (bankruptcy) parser and client tests — no network."""

import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from sources.fedresurs import (  # noqa: E402
    FedresursClient, parse_companies, find_by_inn, is_bankrupt_status,
    bankruptcy_facts, REFERER,
)

FIXTURES = Path(__file__).parent / "fixtures"


def _config():
    return {"throttling": {"fedresurs_min_interval_seconds": 0,
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
        self.headers = {}
        self.last_params = None

    def get(self, url, params=None, timeout=None):
        self.last_params = params
        return _Response(self._payload)


def _companies():
    payload = json.loads((FIXTURES / "fedresurs_companies.json").read_text("utf-8"))
    return parse_companies(payload)


def test_parse_companies_normalizes_rows():
    companies = _companies()
    assert len(companies) == 3
    by_inn = {c["inn"]: c for c in companies}
    assert by_inn["7707001234"]["guid"] == "11111111-2222-3333-4444-555555555555"
    assert by_inn["7707001234"]["active"] is True
    assert by_inn["7707004321"]["active"] is False


def test_find_by_inn_exact():
    companies = _companies()
    assert find_by_inn(companies, "7707001234")["name"] == 'ООО "Ромашка"'
    assert find_by_inn(companies, "770700123") is None


def test_bankruptcy_status_markers():
    assert is_bankrupt_status("Находится в стадии конкурсного производства")
    assert is_bankrupt_status("Введена процедура наблюдения")
    assert is_bankrupt_status("Признано банкротом")
    assert not is_bankrupt_status("Действующее")
    assert not is_bankrupt_status("")


def test_bankruptcy_facts_bankrupt_contract():
    company = find_by_inn(_companies(), "7707004321")
    facts = bankruptcy_facts(company, True,
                             retrieved_at="2026-10-18T00:00:00+00:00")
    by_field = {f.field: f for f in facts}
    assert by_field["bankruptcy.in_registry"].value is True
    assert by_field["bankruptcy.is_bankrupt"].value is True
    assert "конкурсного производства" in by_field["bankruptcy.status"].value
    assert by_field["bankruptcy.guid"].value == company["guid"]
    assert all(f.source == "fedresurs" for f in facts)


def test_bankruptcy_facts_clean_company():
    company = find_by_inn(_companies(), "7707001234")
    facts = bankruptcy_facts(company, True)
    by_field = {f.field: f.value for f in facts}
    assert by_field["bankruptcy.is_bankrupt"] is False


def test_bankruptcy_facts_not_found_is_indirect():
    (fact,) = bankruptcy_facts(None, False)
    assert fact.field == "bankruptcy.in_registry"
    assert fact.value is False
    assert fact.confidence == "indirect"


def test_client_sends_referer_and_filters_exact_inn():
    payload = json.loads((FIXTURES / "fedresurs_companies.json").read_text("utf-8"))
    session = _Session(payload)
    client = FedresursClient(_config(), session=session)
    assert session.headers["Referer"] == REFERER  # mandatory header (WAF)
    company, found = client.lookup_inn("7707001234")
    # server returned unrelated rows too — client picks the exact INN match
    assert found and company["inn"] == "7707001234"
    assert session.last_params["code"] == "7707001234"
    assert session.last_params["limit"] == 15
