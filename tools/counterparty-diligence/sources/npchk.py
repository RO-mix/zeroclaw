"""FNS taxpayer status check (npchk.nalog.ru), SOAP 1.1, no API key.

Endpoint: https://npchk.nalog.ru/FNSNDSCAWS_2?wsdl, method NdsRequest2
(soapAction "NdsRequest2", per the WSDL binding). Implemented manually over
requests (no zeep): static SOAP envelope, one <NP> per INN+KPP pair (up to
10000 per request).

Envelope and record shapes follow the official XSD (?xsd=1/?xsd=2, verified
live 2026-10-17) — note they differ from popular blog examples:
  request:  <NP INN="..." KPP="..." DT="ДД.ММ.ГГГГ"/>   (ATTRIBUTES, not child elements)
  response: <NdsResponse2 DTActUL=".." DTActFL=".." errMsg="..">
              <NP INN=".." KPP=".." DT=".." State="0..12"/>
A wrong request namespace/shape is rejected with a schema validation fault,
and the WAF answers 403 to non-browser User-Agents.

Response state codes (per the XSD documentation):
  0  = registered and active on the request date (±6 days)
  1  = registered but NOT active around the request date
  2  = registered (no date given in the request) — clean
  3  = KPP does not match the INN
  4  = no taxpayer with this INN
  5+ = extended codes; treated as unverified -> red flag
"""

from __future__ import annotations

import re
import warnings
from datetime import date
from typing import Any, Optional
from xml.etree import ElementTree as ET

from .base import (
    BROWSER_USER_AGENT, Fact, RateLimiter, http_timeout, make_session,
    utc_now_iso,
)

WSDL_URL = "https://npchk.nalog.ru/FNSNDSCAWS_2?wsdl"
ENDPOINT_URL = "https://npchk.nalog.ru/FNSNDSCAWS_2"
SOURCE_NAME = "npchk"

SOAP_ACTION = "NdsRequest2"  # from the WSDL soap:operation binding
SOAP_NS = "http://schemas.xmlsoap.org/soap/envelope/"
REQUEST_NS = "http://ws.unisoft/FNSNDSCAWS2/Request"
RESPONSE_NS = "http://ws.unisoft/FNSNDSCAWS2/Response"

OK_STATES = {"0", "2"}  # 2 = registered, date not requested (clean)

STATE_COMMENTS = {
    "0": "NDS0: зарегистрирован, действующий на дату запроса",
    "1": "NDS1: зарегистрирован, но не действующий (±6 дней от даты запроса)",
    "2": "NDS2: зарегистрирован (дата в запросе не указана)",
    "3": "NDS3: КПП не соответствует ИНН",
    "4": "NDS4: налогоплательщик с таким ИНН не зарегистрирован",
}

_ENVELOPE = """<?xml version="1.0" encoding="UTF-8"?>
<soap:Envelope xmlns:soap="{soap_ns}">
  <soap:Body>
    <NdsRequest2 xmlns="{request_ns}">
{records}
    </NdsRequest2>
  </soap:Body>
</soap:Envelope>
"""

_RECORD = '      <NP INN="{inn}" KPP="{kpp}" DT="{date}"/>'


def _fmt_date(value: Optional[str]) -> str:
    """Accept YYYY-MM-DD (ISO) or DD.MM.YYYY; the API wants DD.MM.YYYY."""
    if not value:
        return date.today().strftime("%d.%m.%Y")
    if re.fullmatch(r"\d{4}-\d{2}-\d{2}", value):
        return date.fromisoformat(value).strftime("%d.%m.%Y")
    return value


def build_envelope(requests_: list[dict], request_date: Optional[str] = None) -> str:
    """Build the NdsRequest2 SOAP envelope.

    requests_: list of {"inn": str, "kpp": str}; kpp may be empty.
    request_date: DD.MM.YYYY (YYYY-MM-DD also accepted); defaults to today.
    """
    date_str = _fmt_date(request_date)
    records = "\n".join(
        _RECORD.format(inn=r["inn"], kpp=r.get("kpp", ""), date=date_str)
        for r in requests_[:10000]
    )
    return _ENVELOPE.format(soap_ns=SOAP_NS, request_ns=REQUEST_NS,
                            records=records)


def _local_name(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def parse_response(xml_text: str) -> list[dict]:
    """Parse NdsResponse2: one record per requested INN/KPP, State attribute.

    Namespace-agnostic (local names only). Response records are <NP>
    elements whose INN/KPP/DT/State are ATTRIBUTES per the official XSD.
    """
    root = ET.fromstring(xml_text)
    records = []
    for np_ in root.iter():
        if _local_name(np_.tag) != "NP":
            continue
        state = (np_.get("State") or "").strip()
        if not state:  # a request-side NP has no State; stay defensive
            continue
        records.append({
            "inn": (np_.get("INN") or "").strip(),
            "kpp": (np_.get("KPP") or "").strip(),
            "date": (np_.get("DT") or "").strip(),
            "state": state,
            "comment": STATE_COMMENTS.get(
                state, f"unknown/extended state {state}"),
        })
    return records


def state_fact(record: dict, retrieved_at: Optional[str] = None) -> Fact:
    """A record with state outside OK_STATES is a red flag fed into scoring."""
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
        # TLS to npchk is fragile on intercepted/RU-CA hosts: bypass ambient
        # proxy env by default and keep SSL verification configurable, but
        # NEVER disabled silently (config defaults preserve both).
        npchk_cfg = config.get("npchk", {})
        self._verify_ssl = bool(npchk_cfg.get("verify_ssl", True))
        if session is None:
            session = make_session(config)
            if npchk_cfg.get("direct_no_proxy", True):
                session.trust_env = False
        self._session = session
        self._session.headers["User-Agent"] = BROWSER_USER_AGENT

    def query(self, requests_: list[dict],
              request_date: Optional[str] = None) -> list[dict]:
        envelope = build_envelope(requests_, request_date=request_date)
        self._limiter.wait()
        kwargs = {
            "data": envelope.encode("utf-8"),
            "headers": {
                "Content-Type": "text/xml; charset=utf-8",
                "SOAPAction": SOAP_ACTION,
            },
            "timeout": http_timeout(self.config),
            "verify": self._verify_ssl,
        }
        if self._verify_ssl:
            resp = self._session.post(ENDPOINT_URL, **kwargs)
        else:
            # Conscious opt-out: silence the warning locally, not globally.
            with warnings.catch_warnings():
                warnings.simplefilter("ignore")
                from urllib3.exceptions import InsecureRequestWarning
                warnings.simplefilter("ignore", InsecureRequestWarning)
                resp = self._session.post(ENDPOINT_URL, **kwargs)
        resp.raise_for_status()
        return parse_response(resp.text)

    def check_inn(self, inn: str, kpp: str = "") -> Optional[dict]:
        records = self.query([{"inn": inn, "kpp": kpp}])
        return records[0] if records else None
