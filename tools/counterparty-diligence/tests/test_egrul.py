import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from sources.egrul import (  # noqa: E402
    parse_search_result, company_facts, EgrulClient, decode_body,
)

FIXTURES = Path(__file__).parent / "fixtures"


def _config():
    return {"throttling": {"egrul_min_interval_seconds": 0,
                           "http_timeout_seconds": 5},
            "http": {"trust_env": False}}


class _Response:
    """Fake response; encode payload as cp1251 like the real FNS API."""

    status_code = 200

    def __init__(self, payload=None, text=""):
        if payload is not None:
            text = json.dumps(payload, ensure_ascii=False)
        self.text = text
        self.content = text.encode("cp1251")

    def raise_for_status(self):
        pass


def test_parse_search_result_maps_row_keys():
    payload = json.loads((FIXTURES / "egrul_search_result.json").read_text("utf-8"))
    companies = parse_search_result(payload)
    assert len(companies) == 1
    c = companies[0]
    # Verified live schema (2026-10-17, INN 7736207543):
    assert c["inn"] == "7736207543"
    assert c["kpp"] == "770401001"
    assert c["ogrn"] == "1027700229193"      # key "o"
    assert "Яндекс" in c["name"]             # key "n" = full name
    assert c["short_name"] == 'ООО "Яндекс"'  # key "c"
    assert c["registration_date"] == "18.09.2002"  # key "r"
    assert c["region"] == "г.Москва"         # key "rn"
    assert c["entity_kind"] == "ul"          # key "k"
    assert c["token"]                        # key "t" (vyp-download)
    # not present in this result set -> honest absence, not guessing
    assert c.get("address") is None
    assert c.get("management") is None
    assert c.get("status") is None
    # paging counters must NOT leak into requisites
    assert "management" not in c or c["management"] != "1"
    assert c.get("ogrn") != "18.09.2002"


def test_status_code_translated_when_present():
    payload = {"rows": [{"i": "1", "s": "1"}, {"i": "2", "s": "2"}]}
    companies = parse_search_result(payload)
    assert companies[0]["status"] == "Действует"
    assert companies[1]["status"] == "Ликвидировано"


def test_parse_empty_payload():
    assert parse_search_result({}) == []
    assert parse_search_result({"rows": []}) == []


def test_decode_body_falls_back_to_cp1251():
    resp = _Response(payload={"n": 'ООО "Яндекс"'})
    assert "Яндекс" in decode_body(resp)


def test_company_facts_contract():
    payload = json.loads((FIXTURES / "egrul_search_result.json").read_text("utf-8"))
    facts = company_facts(parse_search_result(payload)[0])
    by_field = {f.field: f for f in facts}
    assert by_field["inn"].value == "7736207543"
    assert by_field["ogrn"].value == "1027700229193"
    assert "short_name" in by_field
    assert by_field["inn"].source == "egrul"
    assert by_field["inn"].confidence == "confirmed"
    assert "retrieved_at" in by_field["inn"].to_dict()


def test_search_end_to_end_with_fake_session():
    payload = json.loads((FIXTURES / "egrul_search_result.json").read_text("utf-8"))

    class FakeSession:
        def post(self, url, data=None, timeout=None):
            assert data["query"] == "7736207543"
            return _Response(payload={"t": "tok1"})

        def get(self, url, timeout=None):
            assert "search-result/tok1" in url
            return _Response(payload=payload)

    client = EgrulClient(_config(), session=FakeSession())
    companies = client.search("7736207543")
    assert companies[0]["inn"] == "7736207543"
    assert companies[0]["ogrn"] == "1027700229193"


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
