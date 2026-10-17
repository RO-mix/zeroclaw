from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from sources.npchk import (  # noqa: E402
    build_envelope, parse_response, state_fact, OK_STATES, NpchkClient,
)

FIXTURES = Path(__file__).parent / "fixtures"


def _config():
    return {"throttling": {"npchk_min_interval_seconds": 0,
                           "http_timeout_seconds": 5},
            "http": {"trust_env": False}}


def test_build_envelope_attributes_and_iso_date_conversion():
    xml = build_envelope([{"inn": "7708004767", "kpp": "770801001"},
                          {"inn": "7707001234", "kpp": ""}],
                         request_date="2026-10-17")
    assert xml.count("<NP ") == 2
    # INN/KPP/DT are ATTRIBUTES per the official XSD; date is DD.MM.YYYY
    assert 'INN="7708004767"' in xml
    assert 'KPP="770801001"' in xml
    assert 'KPP=""' in xml
    assert 'DT="17.10.2026"' in xml
    assert "NdsRequest2" in xml
    assert "http://ws.unisoft/FNSNDSCAWS2/Request" in xml


def test_parse_response_real_fixture():
    xml = (FIXTURES / "npchk_response.xml").read_text("utf-8")
    records = parse_response(xml)
    assert [r["inn"] for r in records] == ["7736207543", "0000000000"]
    assert records[0]["state"] == "0"
    assert "действующий" in records[0]["comment"]
    assert records[0]["kpp"] == "770401001"
    assert records[0]["date"] == "17.10.2026"
    assert records[1]["state"] == "5"
    assert "unknown/extended" in records[1]["comment"]


def test_ok_states():
    assert "0" in OK_STATES and "2" in OK_STATES  # 2 = registered, no date
    assert "1" not in OK_STATES and "3" not in OK_STATES


def test_parse_response_ignores_request_side_np():
    # A request-shaped <NP> (no State attribute) must not become a record.
    xml = ('<?xml version="1.0"?><soap:Envelope '
           'xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/">'
           '<soap:Body><NdsResponse2 xmlns="http://ws.unisoft/FNSNDSCAWS2/Response">'
           '<NP INN="1" KPP="2" DT="d"/>'
           '<NP INN="1" KPP="2" DT="d" State="0"/>'
           "</NdsResponse2></soap:Body></soap:Envelope>")
    records = parse_response(xml)
    assert len(records) == 1 and records[0]["state"] == "0"


def test_state_fact_carries_value():
    record = {"inn": "7707001234", "state": "1"}
    fact = state_fact(record)
    assert fact.field == "npchk.state"
    assert fact.value == "1"
    assert fact.source == "npchk"


def test_query_with_fake_session_posts_soap():
    xml = (FIXTURES / "npchk_response.xml").read_text("utf-8")

    class FakeSession:
        headers = {}

        def post(self, url, data=None, headers=None, timeout=None,
                 verify=None):
            assert "npchk.nalog.ru" in url
            assert headers["SOAPAction"] == "NdsRequest2"
            assert 'INN="7708004767"' in data.decode("utf-8")
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


def test_browser_user_agent_set_on_session():
    from sources.base import BROWSER_USER_AGENT
    client = NpchkClient(_config())
    assert client._session.headers["User-Agent"] == BROWSER_USER_AGENT


def test_verify_ssl_defaults_true_and_is_passed_to_post():
    config = _config()
    config["npchk"] = {"direct_no_proxy": True, "verify_ssl": False}
    seen = {}

    class FakeSession:
        trust_env = True
        headers = {}

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


def test_insecure_warning_suppressed_when_verify_off():
    import warnings
    config = _config()
    config["npchk"] = {"direct_no_proxy": True, "verify_ssl": False}

    class ExplodingSession:
        headers = {}

        def post(self, url, **kwargs):
            # Simulate urllib3 emitting InsecureRequestWarning during the post
            from urllib3.exceptions import InsecureRequestWarning
            warnings.warn("unverified https", InsecureRequestWarning)
            return type("R", (), {
                "raise_for_status": lambda s: None,
                "text": ('<?xml version="1.0"?>'
                         '<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"/>'),
            })()

    client = NpchkClient(config, session=ExplodingSession())
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        client.query([{"inn": "1", "kpp": ""}])
    assert not [w for w in caught
                if "unverified https" in str(w.message)], \
        "InsecureRequestWarning must be suppressed locally"


def test_warning_not_suppressed_when_verify_on():
    import warnings
    config = _config()
    config["npchk"] = {"direct_no_proxy": True, "verify_ssl": True}

    class ExplodingSession:
        headers = {}

        def post(self, url, **kwargs):
            warnings.warn("plain warning", UserWarning)
            return type("R", (), {
                "raise_for_status": lambda s: None,
                "text": ('<?xml version="1.0"?>'
                         '<soap:Envelope xmlns:soap="http://schemas.xmlsoap.org/soap/envelope/"/>'),
            })()

    client = NpchkClient(config, session=ExplodingSession())
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        client.query([{"inn": "1", "kpp": ""}])
    assert any("plain warning" in str(w.message) for w in caught)
