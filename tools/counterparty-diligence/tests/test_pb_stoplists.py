import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from sources.pb_stoplists import (  # noqa: E402
    parse_search_result, extract_stoplist_flags, flag_facts, PbClient,
    PbUnavailableError,
)

FIXTURES = Path(__file__).parent / "fixtures"


def _config():
    return {"throttling": {"pb_min_interval_seconds": 0,
                           "http_timeout_seconds": 5},
            "http": {"trust_env": False}}


class _Response:
    def __init__(self, payload=None, status=200):
        self.status_code = status
        text = json.dumps(payload, ensure_ascii=False) if payload is not None else ""
        self.text = text
        self.content = text.encode("cp1251")

    def raise_for_status(self):
        if self.status_code >= 400:
            raise RuntimeError(f"HTTP {self.status_code}")


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
        def __init__(self):
            self.headers = {}

        def get(self, url, params=None, timeout=None):
            if url == "https://pb.nalog.ru/":
                return _Response(payload="<html>front page</html>")
            assert "search-result.json" in url
            assert params["token"] == "tok"
            return _Response(payload=payload)

        def post(self, url, data=None, timeout=None):
            assert "search-proc.json" in url
            assert data["mode"] == "search"
            assert data["region"] == ""  # country-wide, no region
            return _Response(payload={"token": "tok"})

    client = PbClient(_config(), session=FakeSession())
    cards = client.search("7707001234")
    assert cards[0]["flags"], "expected flags on the dirty fixture company"


def test_primes_front_page_once_for_cookies():
    calls = {"front": 0}

    class FakeSession:
        def __init__(self):
            self.headers = {}

        def get(self, url, params=None, timeout=None):
            if url == "https://pb.nalog.ru/":
                calls["front"] += 1
                return _Response(payload="html")
            return _Response(payload={"rows": []})

        def post(self, url, data=None, timeout=None):
            return _Response(payload={"token": "tok"})

    client = PbClient(_config(), session=FakeSession())
    client.search("7707001234")
    client.search("7707001234")
    assert calls["front"] == 1  # JSESSIONID priming happens once


def test_server_500_raises_with_decoded_reason():
    class FakeSession:
        def __init__(self):
            self.headers = {}

        def get(self, url, params=None, timeout=None):
            return _Response(payload="html")

        def post(self, url, data=None, timeout=None):
            return _Response(payload={"ERROR": "Внутренняя ошибка сервера",
                                      "STATUS": 500,
                                      "DETAILS": "abc"},
                             status=500)

    client = PbClient(_config(), session=FakeSession())
    try:
        client.search("7707001234")
        assert False, "expected PbUnavailableError"
    except PbUnavailableError as exc:
        assert "500" in str(exc)
        assert "Внутренняя ошибка" in str(exc)
