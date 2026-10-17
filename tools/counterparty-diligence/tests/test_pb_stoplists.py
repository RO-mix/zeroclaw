import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from sources.pb_stoplists import (  # noqa: E402
    parse_search_result, extract_stoplist_flags, flag_facts, PbClient,
)

FIXTURES = Path(__file__).parent / "fixtures"


def _config():
    return {"throttling": {"pb_min_interval_seconds": 0,
                           "http_timeout_seconds": 5},
            "http": {"trust_env": False}}


def test_parse_search_result_extracts_flags_from_markers():
    payload = json.loads((FIXTURES / "pb_search_result.json").read_text("utf-8"))
    cards = parse_search_result(payload)
    assert len(cards) == 2
    assert cards[0]["inn"] == "7707001234"
    assert "mass_address" in cards[0]["flags"]
    assert "tax_debt" in cards[0]["flags"]
    assert cards[1]["flags"] == []


def test_extract_flags_from_company_card():
    card = {"raw": {"status": ["Недостоверный паспорт руководителя"]}}
    flags = extract_stoplist_flags(card)
    assert "unreliable_passport" in flags


def test_flag_facts_contract():
    facts = flag_facts(["mass_address"], inn="7707001234")
    assert facts[0].field == "stoplist.mass_address"
    assert facts[0].source == "pb_stoplists"
    assert facts[0].value is True


def test_search_end_to_end_with_fake_session():
    payload = json.loads((FIXTURES / "pb_search_result.json").read_text("utf-8"))

    class FakeSession:
        def post(self, url, data=None, timeout=None):
            assert "search-proc.json" in url
            assert data["mode"] == "search"
            assert data["region"] == ""  # country-wide, no region
            return type("R", (), {"raise_for_status": lambda s: None,
                                  "json": lambda s: {"token": "tok"}})()

        def get(self, url, params=None, timeout=None):
            assert "search-result.json" in url
            assert params["token"] == "tok"
            return type("R", (), {"raise_for_status": lambda s: None,
                                  "json": lambda s: payload})()

    client = PbClient(_config(), session=FakeSession())
    cards = client.search("7707001234")
    assert cards[0]["flags"], "expected flags on the dirty fixture company"
