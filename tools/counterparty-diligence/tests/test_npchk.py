from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from sources.npchk import (  # noqa: E402
    build_envelope, parse_response, state_fact, NpchkClient,
)

FIXTURES = Path(__file__).parent / "fixtures"


def _config():
    return {"throttling": {"npchk_min_interval_seconds": 0,
                           "http_timeout_seconds": 5},
            "http": {"trust_env": False}}


def test_build_envelope_contains_records_and_date():
    xml = build_envelope([{"inn": "7708004767", "kpp": "770801001"},
                          {"inn": "7707001234", "kpp": ""}],
                         request_date="2026-10-17")
    assert xml.count("<NP>") == 2
    assert "<INN>7708004767</INN>" in xml
    assert "<KPP></KPP>" in xml
    assert "<Date>2026-10-17</Date>" in xml
    assert "NdsRequest2" in xml


def test_parse_response_states_and_comments():
    xml = (FIXTURES / "npchk_response.xml").read_text("utf-8")
    records = parse_response(xml)
    assert [r["inn"] for r in records] == ["7708004767", "7707001234",
                                           "7707009999"]
    assert records[0]["state"] == "0"
    assert "действует" in records[0]["comment"]
    assert records[1]["state"] == "2"
    assert "недобросовестных" in records[1]["comment"]
    assert records[2]["state"] == "5"
    assert "ИНН/КПП" in records[2]["comment"]


def test_state_fact_carries_value():
    record = {"inn": "7707001234", "state": "2"}
    fact = state_fact(record)
    assert fact.field == "npchk.state"
    assert fact.value == "2"
    assert fact.source == "npchk"


def test_query_with_fake_session_posts_soap():
    xml = (FIXTURES / "npchk_response.xml").read_text("utf-8")

    class FakeSession:
        def post(self, url, data=None, headers=None, timeout=None,
                 verify=None):
            assert "npchk.nalog.ru" in url
            assert headers["SOAPAction"] == "NdsRequest2"
            assert b"<INN>7708004767</INN>" in data
            return type("R", (), {
                "raise_for_status": lambda s: None,
                "text": xml,
            })()

    client = NpchkClient(_config(), session=FakeSession())
    records = client.query([{"inn": "7708004767", "kpp": "770801001"}])
    assert records[0]["state"] == "0"


def test_direct_no_proxy_disables_trust_env():
    config = _config()
    config["npchk"] = {"direct_no_proxy": True, "verify_ssl": True}
    client = NpchkClient(config)
    assert client._session.trust_env is False


def test_verify_ssl_defaults_true_and_is_passed_to_post():
    config = _config()
    config["npchk"] = {"direct_no_proxy": True, "verify_ssl": False}
    seen = {}

    class FakeSession:
        trust_env = True

        def post(self, url, data=None, headers=None, timeout=None, verify=None):
            seen["verify"] = verify
            empty_ok = ('<?xml version="1.0"?>'
                        '<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"/>')
            return type("R", (), {"raise_for_status": lambda s: None,
                                  "text": empty_ok})()

    client = NpchkClient(config, session=FakeSession())
    assert client._verify_ssl is False
    client.query([{"inn": "1", "kpp": ""}])
    assert seen["verify"] is False
