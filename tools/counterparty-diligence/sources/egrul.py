"""FNS EGRUL adapter (egrul.nalog.ru): search by INN or name.

Flow: POST https://egrul.nalog.ru/ (form query=..., region=) -> {"t": token}
-> GET https://egrul.nalog.ru/search-result/{token} -> JSON rows of requisites.
Optional PDF extract: GET /vyp-download/{token} (kept out of M1 scope; the
adapter only returns the URL).

Row schema verified live on 2026-10-17 (INN 7736207543). The API serves
cp1251 JSON without a charset header. Single-letter row keys:
  i  = INN, p = KPP, o = OGRN (13 digits), r = registration date (DD.MM.YYYY)
  n  = full name, c = short name (e.g. ООО "Яндекс")
  s  = status code ("1" active, "2" liquidated; absent on some rows)
  e  = address, g = GRN (when present in the result set)
  rn = region, k = entity kind ("ul" = legal entity, "fl" = individual)
  pg/cnt/tot = paging counters, t = per-row token (used by /vyp-download)
"""

from __future__ import annotations

import json
from typing import Any, Optional

from .base import Fact, RateLimiter, http_timeout, make_session, utc_now_iso

BASE_URL = "https://egrul.nalog.ru"
SOURCE_NAME = "egrul"

_ROW_KEY_MAP = {
    "i": "inn",
    "p": "kpp",
    "o": "ogrn",
    "n": "name",
    "c": "short_name",
    "r": "registration_date",
    "e": "address",
    "g": "grn",
    "s": "status",
    "rn": "region",
    "k": "entity_kind",
}

_STATUS_LABELS = {"1": "Действует", "2": "Ликвидировано"}


def decode_body(resp: Any) -> str:
    """FNS serves cp1251 without charset; try utf-8 first, fall back."""
    try:
        return resp.content.decode("utf-8")
    except UnicodeDecodeError:
        return resp.content.decode("cp1251")


def parse_search_result(payload: dict) -> list[dict]:
    """Normalize the raw /search-result/{token} JSON into company dicts."""
    rows = payload.get("rows") or []
    companies = []
    for row in rows:
        company = {dst: row.get(src) for src, dst in _ROW_KEY_MAP.items()
                   if src in row}
        company["token"] = row.get("t")
        company["raw"] = row
        reg = company.get("registration_date")
        if isinstance(reg, str) and " " in reg:
            company["registration_date"] = reg.split(" ")[0]
        status = company.get("status")
        if status is not None:
            company["status"] = _STATUS_LABELS.get(str(status), str(status))
        companies.append(company)
    return companies


def company_facts(company: dict, retrieved_at: Optional[str] = None) -> list[Fact]:
    """Convert a parsed company dict into the unified fact contract."""
    retrieved_at = retrieved_at or utc_now_iso()
    url = f"{BASE_URL}/"
    facts = []
    for field in ("name", "short_name", "inn", "kpp", "ogrn", "address",
                  "management", "okved", "registration_date", "status"):
        value = company.get(field)
        if value not in (None, ""):
            facts.append(Fact(field=field, value=value, source=SOURCE_NAME,
                              source_url=url, retrieved_at=retrieved_at))
    return facts


class EgrulClient:
    def __init__(self, config: dict, session: Optional[Any] = None):
        self.config = config
        interval = float(config.get("throttling", {}).get(
            "egrul_min_interval_seconds", 30))
        self._limiter = RateLimiter(interval)
        self._session = session or make_session(config)

    def search_token(self, query: str, region: str = "") -> str:
        """POST the search form; return the result token."""
        self._limiter.wait()
        resp = self._session.post(
            f"{BASE_URL}/",
            data={"query": query, "region": region},
            timeout=http_timeout(self.config),
        )
        resp.raise_for_status()
        token = json.loads(decode_body(resp)).get("t")
        if not token:
            raise ValueError("egrul search returned no token")
        return token

    def get_result(self, token: str) -> dict:
        """Fetch /search-result/{token}; one retry on an empty body."""
        for attempt in range(2):
            self._limiter.wait()
            resp = self._session.get(
                f"{BASE_URL}/search-result/{token}",
                timeout=http_timeout(self.config),
            )
            resp.raise_for_status()
            try:
                payload = json.loads(decode_body(resp))
            except ValueError:
                payload = {}
            if payload:
                return payload
        return {}

    def search(self, query: str, region: str = "") -> list[dict]:
        token = self.search_token(query, region=region)
        return parse_search_result(self.get_result(token))

    def resolve_inn(self, query: str) -> Optional[dict]:
        """Find a single unambiguous company; None on zero or many matches."""
        companies = self.search(query)
        return companies[0] if len(companies) == 1 else None

    def vyp_download_url(self, token: str) -> str:
        """PDF extract lives outside M1; only expose the URL (PRD)."""
        return f"{BASE_URL}/vyp-download/{token}"
