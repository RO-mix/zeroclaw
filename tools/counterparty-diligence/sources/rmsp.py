"""FNS SME registry adapter (rmsp.nalog.ru — Единый реестр субъектов МСП).

Flow (verified live 2026-10-18, direct from RU network):
GET https://rmsp.nalog.ru/search-proc.json?query=<ИНН или фрагмент имени>
-> 200 JSON {"data": [...], "rowCount": N, ...} — no token dance, unlike egrul.

Row schema (captured from a live response):
  inn        = ИНН
  ogrn       = ОГРН
  category   = 1 micro | 2 small | 3 medium (Постановление Правительства
               РФ № 265 «Об утверждении Правил отнесения...»)
  dtregistry = inclusion datetime "DD.MM.YYYY HH:MM:SS"
  is_active  = 1 (in registry) — rows may carry is_active=0 for excluded
  nptype     = "UL" (legal entity) | "IP" (individual entrepreneur)
  okved1 / okved1name = main OKVED code and name
  regioncode, name_ex (extended name), token (row-level)

Empty `data` for an exact INN means "not in the SME registry" — a factual
result, not an error (verified: large companies like ИНН 7736207543 /
7713011337 return rowCount 0). Cyrillic fields may arrive cp1251-encoded;
decode with the utf-8 -> cp1251 fallback used by the FNS adapters.
"""

from __future__ import annotations

import json
from typing import Any, Optional

from .base import Fact, RateLimiter, http_timeout, make_session, utc_now_iso

BASE_URL = "https://rmsp.nalog.ru"
SOURCE_NAME = "rmsp"

CATEGORY_LABELS = {1: "Микропредприятие", 2: "Малое предприятие",
                   3: "Среднее предприятие"}


def decode_body(resp: Any) -> str:
    try:
        return resp.content.decode("utf-8")
    except UnicodeDecodeError:
        return resp.content.decode("cp1251")


def parse_search_result(payload: dict) -> list[dict]:
    """Normalize raw search-proc.json rows into SME registry entries."""
    rows = payload.get("data") or []
    entries = []
    for row in rows:
        category = row.get("category")
        try:
            category = int(category)
        except (TypeError, ValueError):
            category = None
        dt = row.get("dtregistry") or ""
        entries.append({
            "inn": row.get("inn"),
            "ogrn": row.get("ogrn"),
            "category": category,
            "category_label": CATEGORY_LABELS.get(category),
            "inclusion_date": dt.split(" ")[0] if dt else "",
            "active": bool(row.get("is_active")),
            "entity_kind": row.get("nptype"),
            "region_code": row.get("regioncode"),
            "okved": row.get("okved1"),
            "raw": row,
        })
    return entries


def find_by_inn(entries: list[dict], inn: str) -> Optional[dict]:
    """Exact-INN match; the search also matches names/substrings."""
    inn = (inn or "").strip()
    for entry in entries:
        if entry.get("inn") == inn:
            return entry
    return None


def msp_facts(entry: Optional[dict], found: bool,
              retrieved_at: Optional[str] = None) -> list[Fact]:
    """Fact contract for the registry check.

    found=True  -> confirmed in-registry facts (category, inclusion date).
    found=False -> confirmed NOT in the registry (valid empty response).
    """
    retrieved_at = retrieved_at or utc_now_iso()
    url = f"{BASE_URL}/search-proc.json?query="
    if not found:
        return [Fact(field="msp.in_registry", value=False, source=SOURCE_NAME,
                     source_url=url, retrieved_at=retrieved_at)]
    facts = [Fact(field="msp.in_registry", value=True, source=SOURCE_NAME,
                  source_url=url, retrieved_at=retrieved_at)]
    if entry:
        if entry.get("category_label"):
            facts.append(Fact(field="msp.category",
                              value=entry["category_label"],
                              source=SOURCE_NAME, source_url=url,
                              retrieved_at=retrieved_at))
        if entry.get("inclusion_date"):
            facts.append(Fact(field="msp.inclusion_date",
                              value=entry["inclusion_date"],
                              source=SOURCE_NAME, source_url=url,
                              retrieved_at=retrieved_at))
        facts.append(Fact(field="msp.active", value=entry.get("active", True),
                          source=SOURCE_NAME, source_url=url,
                          retrieved_at=retrieved_at))
    return facts


class RmspClient:
    """GET-only client for the SME registry (throttled like other FNS hosts)."""

    def __init__(self, config: dict, session: Optional[Any] = None):
        self.config = config
        interval = float(config.get("throttling", {}).get(
            "rmsp_min_interval_seconds", 30))
        self._limiter = RateLimiter(interval)
        self._session = session or make_session(config)

    def search(self, query: str) -> list[dict]:
        """Search by INN, OGRN or name fragment; normalized entries."""
        self._limiter.wait()
        resp = self._session.get(
            f"{BASE_URL}/search-proc.json",
            params={"query": query},
            timeout=http_timeout(self.config),
        )
        resp.raise_for_status()
        payload = json.loads(decode_body(resp))
        return parse_search_result(payload)

    def lookup_inn(self, inn: str) -> tuple[Optional[dict], bool]:
        """Return (entry, found) for an exact INN match."""
        entries = self.search(inn)
        entry = find_by_inn(entries, inn)
        return entry, entry is not None

    def facts_for_inn(self, inn: str) -> list[Fact]:
        entry, found = self.lookup_inn(inn)
        return msp_facts(entry, found)
