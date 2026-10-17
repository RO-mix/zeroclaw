"""Digital footprint (whois/RDAP + crt.sh) tests — no network."""

import json
from datetime import date
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from sources.digitalsleuth import (  # noqa: E402
    CrtShUnavailableError, DigitalSleuthClient, age_years, domain_facts,
    parse_crt_sh, parse_rdap_created, parse_whois, parse_whois_created,
)

FIXTURES = Path(__file__).parent / "fixtures"

WHOIS_TEXT = """% TCI Whois Service. Terms of use:
% https://tcinet.ru/documents/whois.pdf

domain:        EXAMPLE.RU
nserver:       ns1.example.ru.
state:         REGISTERED, DELEGATED, VERIFIED
registrar:     RU-CENTER-RU
created:       2020-09-23T09:45:07Z
paid-till:     2027-09-30T21:00:00Z
source:        TCI
"""


def _config(**over):
    cfg = {"throttling": {"digitalsleuth_min_interval_seconds": 0,
                          "http_timeout_seconds": 5},
           "digitalsleuth": {"whois_timeout_seconds": 5, "crtsh_retries": 1}}
    cfg.update(over)
    return cfg


class _Response:
    def __init__(self, payload=None, status=200):
        self.status_code = status
        self.content = json.dumps(payload or {}, ensure_ascii=False).encode("utf-8")

    def raise_for_status(self):
        if self.status_code >= 400:
            raise RuntimeError(f"HTTP {self.status_code}")


class _Session:
    def __init__(self, responses):
        self._responses = list(responses)

    def get(self, url, params=None, timeout=None):
        return self._responses.pop(0)


def test_parse_whois_extracts_fields():
    info = parse_whois(WHOIS_TEXT)
    assert info["created"] == "2020-09-23T09:45:07Z"
    assert info["paid-till"] == "2027-09-30T21:00:00Z"
    assert info["registrar"] == "RU-CENTER-RU"


def test_parse_whois_created_formats():
    assert parse_whois_created({"created": "2020-09-23T09:45:07Z"}) == date(2020, 9, 23)
    assert parse_whois_created({"created": "23.09.2020"}) == date(2020, 9, 23)
    assert parse_whois_created({"created": "2020-09-23"}) == date(2020, 9, 23)
    assert parse_whois_created({"created": "garbage"}) is None
    assert parse_whois_created({}) is None


def test_age_years():
    assert age_years(date(2020, 9, 23), now=date(2026, 9, 23)) == 6.0
    assert age_years(None) is None


def test_parse_crt_sh_uniques_and_wildcards():
    payload = json.loads((FIXTURES / "crtsh_sample.json").read_text("utf-8"))
    names = parse_crt_sh(payload)
    assert names == ["api.example.ru", "example.ru", "mail.example.ru",
                     "www.example.ru"]


def test_parse_rdap_created():
    payload = {"events": [{"eventAction": "registration",
                           "eventDate": "2015-03-01T00:00:00Z"}]}
    assert parse_rdap_created(payload) == date(2015, 3, 1)
    assert parse_rdap_created({"events": []}) is None


def test_domain_facts_with_not_checked():
    facts = domain_facts("example.ru", None, None, None,
                         "whois://whois.tcinet.ru/example.ru",
                         not_checked="crt.sh failed: HTTP 502")
    by_field = {f.field: f.value for f in facts}
    assert by_field["domain.created_date"].startswith("not_checked")
    assert by_field["domain.subdomains_count"].startswith("not_checked")
    assert all(f.confidence == "unverified" for f in facts)


def test_domain_facts_full():
    facts = domain_facts("example.ru", date(2020, 9, 23), "RU-CENTER-RU",
                         ["www.example.ru"], "whois://x")
    by_field = {f.field: f.value for f in facts}
    assert by_field["domain.created_date"] == "2020-09-23"
    assert by_field["domain.age_years"] > 5
    assert by_field["domain.registrar"] == "RU-CENTER-RU"
    assert by_field["domain.subdomains_count"] == 1


def test_crt_sh_client_success():
    payload = json.loads((FIXTURES / "crtsh_sample.json").read_text("utf-8"))
    session = _Session([_Response(payload)])
    client = DigitalSleuthClient(_config(), session=session)
    names = client.crt_sh_subdomains("example.ru")
    assert len(names) == 4


def test_crt_sh_client_retries_once_on_502():
    payload = json.loads((FIXTURES / "crtsh_sample.json").read_text("utf-8"))
    session = _Session([_Response(status=502), _Response(payload)])
    client = DigitalSleuthClient(_config(), session=session)
    assert len(client.crt_sh_subdomains("example.ru")) == 4


def test_crt_sh_client_gives_up_after_retries():
    session = _Session([_Response(status=502), _Response(status=502)])
    client = DigitalSleuthClient(_config(), session=session)
    try:
        client.crt_sh_subdomains("example.ru")
        assert False, "expected CrtShUnavailableError"
    except CrtShUnavailableError as exc:
        assert "crt.sh" in str(exc)
