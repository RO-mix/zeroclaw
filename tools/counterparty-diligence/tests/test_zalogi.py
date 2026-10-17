"""FNP pledge registry (zalogi) parser and client tests — no network.

NOTE: the fixture is modeled on the frontend rendering code (the live
endpoint is CAPTCHA-gated, see sources/zalogi.py) — no live 200 body was
captured.
"""

import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from sources.zalogi import (  # noqa: E402
    ZalogiClient, ZalogiUnavailableError, parse_search_result, zalog_facts,
)

FIXTURES = Path(__file__).parent / "fixtures"


def _config():
    return {"throttling": {"zalogi_min_interval_seconds": 0,
                           "http_timeout_seconds": 5}}


class _Response:
    def __init__(self, payload=None, status=200, body=None):
        self.status_code = status
        if body is not None:
            self.content = body.encode("utf-8")
        else:
            self.content = json.dumps(payload or {}, ensure_ascii=False).encode("utf-8")

    def raise_for_status(self):
        if self.status_code >= 400:
            raise RuntimeError(f"HTTP {self.status_code}")


class _Session:
    def __init__(self, response):
        self._response = response
        self.headers = {}
        self.last_kwargs = None

    def post(self, url, data=None, headers=None, timeout=None):
        self.last_kwargs = {"data": data, "headers": headers}
        return self._response


def _payload():
    return json.loads((FIXTURES / "zalogi_search_result.json").read_text("utf-8"))


def test_parse_search_result_envelope():
    result = parse_search_result(_payload())
    assert result["total"] == 7
    assert len(result["notifications"]) == 2
    note = result["notifications"][0]
    assert note["reg_date"] == "2024-05-11T00:00:00"
    assert note["pledgors"][0]["name"] == 'ООО "Ромашка"'


def test_parse_falls_back_to_row_count():
    result = parse_search_result({"data": [{}], "total": None})
    assert result["total"] == 1


def test_zalog_facts_contract():
    (fact,) = zalog_facts({"total": 7}, retrieved_at="2026-10-18T00:00:00+00:00")
    assert fact.field == "zalog.count"
    assert fact.value == 7
    assert fact.source == "zalogi"


def test_client_posts_russian_organization_filter():
    session = _Session(_Response(_payload()))
    client = ZalogiClient(_config(), session=session)
    result = client.search_by_debtor(inn="7707001234")
    assert result["total"] == 7
    body = json.loads(session.last_kwargs["data"])
    assert body["isPledgorFilter"] is True
    assert body["russianOrganization"]["inn"] == "7707001234"
    assert body["limit"] == 10 and body["offset"] == 0


def test_client_raises_on_403_captcha():
    # Captured live 2026-10-18: 403 + JSON message (cp1251-decodable).
    body = json.dumps({"errorId": "x", "message": "Не пройдена проверка CAPTCHA",
                       "at": "2026-10-17T22:24:44.321+00:00"}, ensure_ascii=False)
    session = _Session(_Response(status=403, body=body))
    client = ZalogiClient(_config(), session=session)
    try:
        client.search_by_debtor(inn="7707001234")
        assert False, "expected ZalogiUnavailableError"
    except ZalogiUnavailableError as exc:
        assert "403" in str(exc)
        assert "CAPTCHA" in str(exc)


def test_client_raises_on_5xx():
    session = _Session(_Response(status=500, body="boom"))
    client = ZalogiClient(_config(), session=session)
    try:
        client.search_by_debtor(inn="7707001234")
        assert False, "expected ZalogiUnavailableError"
    except ZalogiUnavailableError as exc:
        assert "500" in str(exc)
