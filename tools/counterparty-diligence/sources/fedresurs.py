"""Fedresurs adapter (bankruptcies, fedresurs.ru).

Flow (verified live 2026-10-18, direct from RU network):
GET https://fedresurs.ru/backend/companies?limit=15&offset=0&code=<ИНН>
with the Referer: https://fedresurs.ru/ header — without Referer the
backend refuses the request. Rate limit is ~100 req/hour per IP: the
default throttle is 4 s between calls; stay conservative on bulk runs.

Row schema (captured live):
  guid          = company id at fedresurs (used by /backend/sfactmessages
                  and friends; kept for deep-linking)
  inn, ogrn     = identifiers
  name          = company name
  status        = human-readable company status text (bankruptcy stages
                  appear here: "...конкурсного производства", "наблюдение",
                  "банкрот"...)
  isActive      = boolean-ish company activity flag
  egrulAddress, okvedName

Known limitation (verified live 2026-10-18): the server-side `code`
filter did NOT restrict the result set — the endpoint returned 15 rows
of unrelated companies for a specific INN. The client therefore filters
the exact INN match itself; an empty exact match means "no publications
about this company at Fedresurs" (no bankruptcy messages).
"""

from __future__ import annotations

import json
from typing import Any, Optional

from .base import Fact, RateLimiter, http_timeout, make_session, utc_now_iso

BASE_URL = "https://fedresurs.ru"
SOURCE_NAME = "fedresurs"
BACKEND_URL = f"{BASE_URL}/backend/companies"
REFERER = f"{BASE_URL}/"

# Status-text markers of an active bankruptcy procedure (lowercased).
BANKRUPT_MARKERS = ("банкрот", "наблюден", "конкурсн", "внешн",
                    "финансов оздоровлен", "реструктуризац")


def decode_body(resp: Any) -> str:
    try:
        return resp.content.decode("utf-8")
    except UnicodeDecodeError:
        return resp.content.decode("cp1251")


def parse_companies(payload: dict) -> list[dict]:
    """Normalize raw /backend/companies rows."""
    rows = payload.get("pageData") or payload.get("data") or []
    companies = []
    for row in rows:
        companies.append({
            "guid": row.get("guid"),
            "inn": row.get("inn"),
            "ogrn": row.get("ogrn"),
            "name": row.get("name") or row.get("fullName"),
            "status": row.get("status") or "",
            "active": bool(row.get("isActive")),
            "address": row.get("egrulAddress"),
            "raw": row,
        })
    return companies


def find_by_inn(companies: list[dict], inn: str) -> Optional[dict]:
    inn = (inn or "").strip()
    for company in companies:
        if company.get("inn") == inn:
            return company
    return None


def is_bankrupt_status(status: str) -> bool:
    text = (status or "").lower()
    return any(marker in text for marker in BANKRUPT_MARKERS)


def bankruptcy_facts(company: Optional[dict], found: bool,
                     retrieved_at: Optional[str] = None) -> list[Fact]:
    """Fact contract for the bankruptcy check.

    found=True  -> confirmed registry presence; is_bankrupt reflects the
                   status text. Confidence confirmed.
    found=False -> no Fedresurs publications for this INN. Treated as
                   "not in the bankrupt-company registry", confidence
                   indirect: a fresh message may lag indexing.
    """
    retrieved_at = retrieved_at or utc_now_iso()
    if not found:
        return [Fact(field="bankruptcy.in_registry", value=False,
                     source=SOURCE_NAME, source_url=BACKEND_URL,
                     retrieved_at=retrieved_at, confidence="indirect")]
    status = company.get("status", "") if company else ""
    bankrupt = is_bankrupt_status(status)
    conf = "confirmed"
    facts = [
        Fact(field="bankruptcy.in_registry", value=True, source=SOURCE_NAME,
             source_url=BACKEND_URL, retrieved_at=retrieved_at,
             confidence=conf),
        Fact(field="bankruptcy.is_bankrupt", value=bankrupt,
             source=SOURCE_NAME, source_url=BACKEND_URL,
             retrieved_at=retrieved_at, confidence=conf),
    ]
    if status:
        facts.append(Fact(field="bankruptcy.status", value=status,
                          source=SOURCE_NAME, source_url=BACKEND_URL,
                          retrieved_at=retrieved_at, confidence=conf))
    if company and company.get("guid"):
        facts.append(Fact(field="bankruptcy.guid", value=company["guid"],
                          source=SOURCE_NAME,
                          source_url=f"{BASE_URL}/company/{company['guid']}",
                          retrieved_at=retrieved_at, confidence=conf))
    return facts


class FedresursClient:
    """Referer-carrying client with a conservative rate limit (~100 req/h)."""

    _HEADERS = {"Referer": REFERER,
                "Accept": "application/json, text/plain, */*"}

    def __init__(self, config: dict, session: Optional[Any] = None):
        self.config = config
        interval = float(config.get("throttling", {}).get(
            "fedresurs_min_interval_seconds", 4))
        self._limiter = RateLimiter(interval)
        self._session = session or make_session(config)
        self._session.headers.update(self._HEADERS)

    def search_by_code(self, code: str) -> list[dict]:
        """GET /backend/companies filtered by INN/OGRN code (client-side
        exact match is applied by lookup_inn)."""
        self._limiter.wait()
        resp = self._session.get(
            BACKEND_URL,
            params={"limit": 15, "offset": 0, "code": code},
            timeout=http_timeout(self.config),
        )
        resp.raise_for_status()
        return parse_companies(json.loads(decode_body(resp)))

    def lookup_inn(self, inn: str) -> tuple[Optional[dict], bool]:
        companies = self.search_by_code(inn)
        company = find_by_inn(companies, inn)
        return company, company is not None

    def facts_for_inn(self, inn: str) -> list[Fact]:
        company, found = self.lookup_inn(inn)
        return bankruptcy_facts(company, found)
