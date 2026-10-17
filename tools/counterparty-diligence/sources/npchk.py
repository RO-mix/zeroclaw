"""FNS dishonest suppliers check (npchk.nalog.ru), SOAP 1.1, no API key.

Endpoint: https://npchk.nalog.ru/FNSNDSCAWS_2?wsdl, method NdsRequest2.
Implemented manually over requests (no zeep): the request is a small static
SOAP envelope with one <NP> element per INN+KPP pair (up to 1000).

Response state codes (NDS0..NDS5):
  0 = VAT payer active
  1 = liquidated
  2 = on the dishonest suppliers list
  3 = registration address not confirmed
  4 = registration data not confirmed
  5 = INN/KPP do not match
"""

from __future__ import annotations

from datetime import date
from typing import Any, Optional
from xml.etree import ElementTree as ET

from .base import Fact, RateLimiter, http_timeout, make_session, utc_now_iso

WSDL_URL = "https://npchk.nalog.ru/FNSNDSCAWS_2?wsdl"
ENDPOINT_URL = "https://npchk.nalog.ru/FNSNDSCAWS_2"
SOURCE_NAME = "npchk"

SOAP_ACTION = "NdsRequest2"
SOAP_NS = "http://schemas.xmlsoap.org/soap/envelope/"
UNISOFT_NS = "http://ws.unisoft/"

STATE_COMMENTS = {
    "0": "NDS0: действует",
    "1": "NDS1: ликвидировано",
    "2": "NDS2: в списке недобросовестных поставщиков",
    "3": "NDS3: адрес не подтверждён",
    "4": "NDS4: сведения не подтверждены",
    "5": "NDS5: ИНН/КПП не соответствуют друг другу",
}

_ENVELOPE = """<?xml version="1.0" encoding="UTF-8"?>
<soap:Envelope xmlns:soap="{soap_ns}">
  <soap:Body>
    <NdsRequest2 xmlns="{unisoft_ns}">
{records}
    </NdsRequest2>
  </soap:Body>
</soap:Envelope>
"""

_RECORD = ('      <NP><INN>{inn}</INN>'
           '<KPP>{kpp}</KPP><Date>{date}</Date></NP>')


def build_envelope(requests_: list[dict], request_date: Optional[str] = None) -> str:
    """Build the NdsRequest2 SOAP envelope.

    requests_: list of {"inn": str, "kpp": str}; kpp may be empty.
    request_date: YYYY-MM-DD; defaults to today.
    """
    date_str = request_date or date.today().isoformat()
    records = "\n".join(
        _RECORD.format(inn=r["inn"], kpp=r.get("kpp", ""), date=date_str)
        for r in requests_[:1000]
    )
    return _ENVELOPE.format(soap_ns=SOAP_NS, unisoft_ns=UNISOFT_NS,
                            records=records)


def parse_response(xml_text: str) -> list[dict]:
    """Parse NdsResponse2: one record per requested INN/KPP with State code."""
    root = ET.fromstring(xml_text)
    records = []
    for record in root.iter(f"{{{UNISOFT_NS}}}NDSRecord"):
        def text(tag: str) -> str:
            el = record.find(f"{{{UNISOFT_NS}}}{tag}")
            return (el.text or "").strip() if el is not None and el.text else ""
        state = text("State")
        records.append({
            "inn": text("INN"),
            "kpp": text("KPP"),
            "date": text("Date"),
            "state": state,
            "comment": STATE_COMMENTS.get(state, f"unknown state {state}"),
        })
    return records


def state_fact(record: dict, retrieved_at: Optional[str] = None) -> Fact:
    """A record with state != 0 is a red flag fed into scoring."""
    retrieved_at = retrieved_at or utc_now_iso()
    return Fact(
        field="npchk.state",
        value=record["state"],
        source=SOURCE_NAME,
        source_url=WSDL_URL,
        retrieved_at=retrieved_at,
        confidence="confirmed",
    )


class NpchkClient:
    def __init__(self, config: dict, session: Optional[Any] = None):
        self.config = config
        interval = float(config.get("throttling", {}).get(
            "npchk_min_interval_seconds", 5))
        self._limiter = RateLimiter(interval)
        self._session = session or make_session(config)

    def query(self, requests_: list[dict],
              request_date: Optional[str] = None) -> list[dict]:
        envelope = build_envelope(requests_, request_date=request_date)
        self._limiter.wait()
        resp = self._session.post(
            ENDPOINT_URL,
            data=envelope.encode("utf-8"),
            headers={
                "Content-Type": "text/xml; charset=utf-8",
                "SOAPAction": SOAP_ACTION,
            },
            timeout=http_timeout(self.config),
        )
        resp.raise_for_status()
        return parse_response(resp.text)

    def check_inn(self, inn: str, kpp: str = "") -> Optional[dict]:
        records = self.query([{"inn": inn, "kpp": kpp}])
        return records[0] if records else None
