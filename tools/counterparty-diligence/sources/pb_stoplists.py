"""FNS stop-lists adapter (pb.nalog.ru "Прозрачный бизнес" stop factors).

Flow mirrors EGRUL: POST https://pb.nalog.ru/search-proc.json
(query, mode=search, region empty = whole country) -> {"token": ...}
-> GET https://pb.nalog.ru/search-result.json?token=... -> cards.

Flags extracted per company: mass address, mass director, unreliable
passport, tax debt ("задолженность").

Known limitation (verified live 2026-10-17): pb.nalog.ru answers
HTTP 500 "Внутренняя ошибка сервера" on search-proc.json even with full
browser mechanics (JSESSIONID, headers, page params). Stop-list flags then
degrade to "not checked" in the pipeline (PRD FR9).
"""

from __future__ import annotations

import json
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


class PbUnavailableError(RuntimeError):
    """pb.nalog.ru failed (HTTP 5xx / no token) with the decoded reason."""


def _decode_body(resp: Any) -> str:
    try:
        return resp.content.decode("utf-8")
    except UnicodeDecodeError:
        return resp.content.decode("cp1251")


class PbClient:
    """Browser-mechanics client: prime cookies with GET /, then AJAX POST.

    Known limitation (verified live 2026-10-17): search-proc.json currently
    answers HTTP 500 "Внутренняя ошибка сервера" (encrypted DETAILS token)
    even with cookies/headers replicated; PbUnavailableError carries the
    exact server message so the pipeline degrades honestly.
    """

    _HEADERS = {
        "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
                      "AppleWebKit/537.36 (KHTML, like Gecko) "
                      "Chrome/129.0 Safari/537.36",
        "Accept": "application/json, text/javascript, */*; q=0.01",
        "X-Requested-With": "XMLHttpRequest",
        "Referer": f"{BASE_URL}/",
    }

    def __init__(self, config: dict, session: Optional[Any] = None):
        self.config = config
        interval = float(config.get("throttling", {}).get(
            "pb_min_interval_seconds", 5))
        self._limiter = RateLimiter(interval)
        self._session = session or make_session(config)
        self._session.headers.update(self._HEADERS)
        self._primed = False

    def _prime(self) -> None:
        """Fetch the front page once per session to obtain JSESSIONID."""
        if self._primed:
            return
        self._limiter.wait()
        resp = self._session.get(f"{BASE_URL}/", timeout=http_timeout(self.config))
        resp.raise_for_status()
        self._primed = True

    @staticmethod
    def _check(resp: Any) -> dict:
        """Raise PbUnavailableError with the decoded server reason on 5xx."""
        if resp.status_code >= 500:
            reason = ""
            try:
                body = json.loads(_decode_body(resp))
                reason = body.get("ERROR") or body.get("DETAILS") or str(body)
            except (ValueError, UnicodeDecodeError):
                reason = _decode_body(resp)[:200]
            raise PbUnavailableError(
                f"pb.nalog.ru HTTP {resp.status_code}: {reason}")
        resp.raise_for_status()
        try:
            return json.loads(_decode_body(resp))
        except ValueError:
            return {}

    def search_token(self, query: str, region: str = "") -> str:
        """POST search-proc.json (country-wide when region is empty)."""
        self._prime()
        self._limiter.wait()
        resp = self._session.post(
            f"{BASE_URL}/search-proc.json",
            data={"query": query, "mode": "search", "region": region,
                  "page": "1", "pageSize": "10"},
            timeout=http_timeout(self.config),
        )
        token = self._check(resp).get("token")
        if not token:
            raise PbUnavailableError("pb search returned no token")
        return token

    def get_result(self, token: str) -> dict:
        self._limiter.wait()
        resp = self._session.get(
            f"{BASE_URL}/search-result.json",
            params={"token": token},
            timeout=http_timeout(self.config),
        )
        return self._check(resp) or {}

    def search(self, query: str, region: str = "") -> list[dict]:
        token = self.search_token(query, region=region)
        return parse_search_result(self.get_result(token))

    def company_card(self, card_id: str, card_hash: str) -> dict:
        """Fetch the full company card (company-proc.json)."""
        self._prime()
        self._limiter.wait()
        resp = self._session.post(
            f"{BASE_URL}/company-proc.json",
            data={"id": card_id, "hash": card_hash},
            timeout=http_timeout(self.config),
        )
        return self._check(resp)

    def stoplist_flags(self, query: str) -> list[str]:
        flags: list[str] = []
        for card in self.search(query):
            for flag in extract_stoplist_flags(card):
                if flag not in flags:
                    flags.append(flag)
        return flags
