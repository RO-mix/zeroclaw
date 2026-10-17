"""FNS EGRUL adapter (egrul.nalog.ru): search by INN or name.

Flow: POST https://egrul.nalog.ru/ (form query=..., region=) -> {"t": token}
-> GET https://egrul.nalog.ru/search-result/{token} -> JSON rows of requisites.
Optional PDF extract: GET /vyp-download/{token} (kept out of M1 scope; the
adapter only returns the URL).
"""

from __future__ import annotations

from typing import Any, Optional

from .base import Fact, RateLimiter, http_timeout, make_session, utc_now_iso

BASE_URL = "https://egrul.nalog.ru"
SOURCE_NAME = "egrul"

# EGRUL search-result row keys are single-letter; map the stable ones.
_ROW_KEY_MAP = {
    "g": "ogrn",
    "i": "inn",
    "p": "kpp",
    "n": "name",
    "c": "address",
    "r": "management",      # e.g. "Генеральный директор ИВАНОВ И.И."
    "e": "okved",
    "k": "okpo",
    "pg": "registration_date",
    "cnt": "row_count",
    "o": "entity_type",
    "s": "status",          # present on some results; often absent
}


def parse_search_result(payload: dict) -> list[dict]:
    """Normalize the raw /search-result/{token} JSON into company dicts."""
    rows = payload.get("rows") or []
    companies = []
    for row in rows:
        company = {dst: row.get(src) for src, dst in _ROW_KEY_MAP.items() if src in row}
        company["raw"] = row
        if company.get("registration_date"):
            # EGRUL returns "YYYY-MM-DD HH:MM:SS"; keep the date part.
            company["registration_date"] = str(company["registration_date"])[:10]
        companies.append(company)
    return companies


def company_facts(company: dict, retrieved_at: Optional[str] = None) -> list[Fact]:
    """Convert a parsed company dict into the unified fact contract."""
    retrieved_at = retrieved_at or utc_now_iso()
    url = f"{BASE_URL}/"
    facts = []
    for field in ("ogrn", "inn", "kpp", "name", "address", "management",
                  "okved", "registration_date", "status"):
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
        token = resp.json().get("t")
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
                payload = resp.json()
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
