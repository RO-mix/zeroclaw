import json
from pathlib import Path

import pytest

import sys
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from sources.egrul import parse_search_result, company_facts, EgrulClient  # noqa: E402

FIXTURES = Path(__file__).parent / "fixtures"


def _config():
    return {"throttling": {"egrul_min_interval_seconds": 0,
                           "http_timeout_seconds": 5},
            "http": {"trust_env": False}}


class _Response:
    def __init__(self, payload=None, text=""):
        self._payload = payload
        self.text = text

    def raise_for_status(self):
        pass

    def json(self):
        if self._payload is None:
            raise ValueError("no json")
        return self._payload


def test_parse_search_result_maps_row_keys():
    payload = json.loads((FIXTURES / "egrul_search_result.json").read_text("utf-8"))
    companies = parse_search_result(payload)
    assert len(companies) == 1
    c = companies[0]
    assert c["inn"] == "7708004767"
    assert c["ogrn"] == "1027700132199"
    assert c["kpp"] == "770801001"
    assert "ЛУКОЙЛ" in c["name"]
    assert c["registration_date"] == "2000-08-08"  # time part trimmed
    assert c["status"] == "Действующее"
    assert c["raw"]["cnt"] == 1


def test_parse_empty_payload():
    assert parse_search_result({}) == []
    assert parse_search_result({"rows": []}) == []


def test_company_facts_contract():
    payload = json.loads((FIXTURES / "egrul_search_result.json").read_text("utf-8"))
    facts = company_facts(parse_search_result(payload)[0])
    by_field = {f.field: f for f in facts}
    assert by_field["inn"].value == "7708004767"
    assert by_field["inn"].source == "egrul"
    assert by_field["inn"].confidence == "confirmed"
    assert "retrieved_at" in by_field["inn"].to_dict()


def test_search_end_to_end_with_fake_session():
    payload = json.loads((FIXTURES / "egrul_search_result.json").read_text("utf-8"))

    class FakeSession:
        def post(self, url, data=None, timeout=None):
            assert data["query"] == "7708004767"
            return _Response(payload={"t": "tok1"})

        def get(self, url, timeout=None):
            assert "search-result/tok1" in url
            return _Response(payload=payload)

    client = EgrulClient(_config(), session=FakeSession())
    companies = client.search("7708004767")
    assert companies[0]["inn"] == "7708004767"


def test_get_result_retries_once_on_empty_body():
    calls = {"n": 0}

    class FakeSession:
        def get(self, url, timeout=None):
            calls["n"] += 1
            return _Response(payload={})

    client = EgrulClient(_config(), session=FakeSession())
    assert client.get_result("tok") == {}
    assert calls["n"] == 2  # initial + one retry


def test_vyp_download_url_only():
    client = EgrulClient(_config(), session=object())
    url = client.vyp_download_url("tok")
    assert url == "https://egrul.nalog.ru/vyp-download/tok"
