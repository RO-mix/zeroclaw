"""FNS stop-lists adapter (pb.nalog.ru "Прозрачный бизнес" stop factors).

Flow mirrors EGRUL: POST https://pb.nalog.ru/search-proc.json
(query, mode=search, region empty = whole country) -> {"token": ...}
-> GET https://pb.nalog.ru/search-result.json?token=... -> cards.

Flags extracted per company: mass address, mass director, unreliable
passport, tax debt ("задолженность").
"""

from __future__ import annotations

from typing import Any, Optional

from .base import Fact, RateLimiter, http_timeout, make_session, utc_now_iso

BASE_URL = "https://pb.nalog.ru"
SOURCE_NAME = "pb_stoplists"

# Marker phrases scanned in the card payload (lowercase).
FLAG_MARKERS = {
    "mass_address": ("массовый адрес", "адрес массовой регистрации"),
    "mass_director": ("массовый руководитель", "массовый директор"),
    "unreliable_passport": ("недостоверный паспорт", "паспорт недостоверен"),
    "tax_debt": ("имеется задолженность", "задолженность по уплате налогов",
                 "не исполнена обязанность по уплате"),
}


def _scan_markers(card: Any) -> list[str]:
    """Return flag keys whose marker phrases appear anywhere in the card."""
    text = str(card).lower()
    return [key for key, markers in FLAG_MARKERS.items()
            if any(m in text for m in markers)]


def parse_search_result(payload: dict) -> list[dict]:
    """Normalize pb search-result rows; keep raw for card-level parsing."""
    rows = payload.get("rows") or payload.get("data") or []
    cards = []
    for row in rows:
        card = {
            "id": row.get("id") or row.get("hid"),
            "hash": row.get("hash"),
            "name": row.get("n"),
            "inn": row.get("i"),
            "ogrn": row.get("g"),
            "flags": _scan_markers(row),
            "raw": row,
        }
        cards.append(card)
    return cards


def extract_stoplist_flags(card: dict) -> list[str]:
    """Flags for one company card (search row or company-proc.json body)."""
    flags = list(card.get("flags") or [])
    for extra in _scan_markers(card.get("raw", {})):
        if extra not in flags:
            flags.append(extra)
    return flags


def flag_facts(flags: list[str], inn: Optional[str],
               retrieved_at: Optional[str] = None) -> list[Fact]:
    retrieved_at = retrieved_at or utc_now_iso()
    return [Fact(field=f"stoplist.{flag}", value=True, source=SOURCE_NAME,
                 source_url=f"{BASE_URL}/", retrieved_at=retrieved_at,
                 confidence="confirmed") for flag in flags]


class PbClient:
    def __init__(self, config: dict, session: Optional[Any] = None):
        self.config = config
        interval = float(config.get("throttling", {}).get(
            "pb_min_interval_seconds", 5))
        self._limiter = RateLimiter(interval)
        self._session = session or make_session(config)

    def search_token(self, query: str, region: str = "") -> str:
        """POST search-proc.json (country-wide when region is empty)."""
        self._limiter.wait()
        resp = self._session.post(
            f"{BASE_URL}/search-proc.json",
            data={"query": query, "mode": "search", "region": region},
            timeout=http_timeout(self.config),
        )
        resp.raise_for_status()
        token = resp.json().get("token")
        if not token:
            raise ValueError("pb search returned no token")
        return token

    def get_result(self, token: str) -> dict:
        self._limiter.wait()
        resp = self._session.get(
            f"{BASE_URL}/search-result.json",
            params={"token": token},
            timeout=http_timeout(self.config),
        )
        resp.raise_for_status()
        return resp.json() or {}

    def search(self, query: str, region: str = "") -> list[dict]:
        token = self.search_token(query, region=region)
        return parse_search_result(self.get_result(token))

    def company_card(self, card_id: str, card_hash: str) -> dict:
        """Fetch the full company card (company-proc.json)."""
        self._limiter.wait()
        resp = self._session.post(
            f"{BASE_URL}/company-proc.json",
            data={"id": card_id, "hash": card_hash},
            timeout=http_timeout(self.config),
        )
        resp.raise_for_status()
        return resp.json() or {}

    def stoplist_flags(self, query: str) -> list[str]:
        flags: list[str] = []
        for card in self.search(query):
            for flag in extract_stoplist_flags(card):
                if flag not in flags:
                    flags.append(flag)
        return flags
