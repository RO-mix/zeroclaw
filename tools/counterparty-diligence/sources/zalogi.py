"""FNP pledge registry adapter (reestr-zalogov.ru).

Discovery (2026-10-18, direct from RU network): the search page
(/search/index) is a Vue app; its bundle posts JSON to
POST https://www.reestr-zalogov.ru/api/search/notary
body: {"isPledgorFilter": true,
       "russianOrganization": {"name": ..., "inn": ..., "ogrn": ...},
       "limit": 10, "offset": 0}
-> {"data": [...], "total": N, "pages": M}; HTTP 403 + {"message":
"Не пройдена проверка CAPTCHA"} when the anti-bot check is not passed.

Known limitation (verified live 2026-10-18): the backend answers 403
CAPTCHA to scripted requests (the browser flow solves it interactively).
Exactly like pb.nalog.ru in M1, this adapter raises ZalogiUnavailableError
with the decoded server reason and the pipeline degrades the pledge
section to "not checked" (PRD FR9). No bypass is implemented on purpose.
If Fedresurs Notariat later publishes a legal API key or the captcha
gate loosens, plug the token into `captcha_token` below.
"""

from __future__ import annotations

import json
from typing import Any, Optional

from .base import Fact, RateLimiter, http_timeout, make_session, utc_now_iso

BASE_URL = "https://www.reestr-zalogov.ru"
SOURCE_NAME = "zalogi"
NOTARY_URL = f"{BASE_URL}/api/search/notary"


class ZalogiUnavailableError(RuntimeError):
    """reestr-zalogov.ru refused the search (CAPTCHA/anti-bot or 5xx)."""


def _decode_body(resp: Any) -> str:
    try:
        return resp.content.decode("utf-8")
    except UnicodeDecodeError:
        return resp.content.decode("cp1251")


def parse_search_result(payload: dict) -> dict:
    """Normalize the notary search response.

    Response envelope (from the frontend bundle): {"data": [...], "total":
    N, "pages": M}. Row fields rendered by the frontend: regDate, link,
    pledgors, pledgees, history. NOTE: no live 200 body was captured (the
    endpoint is CAPTCHA-gated) — the fixture in tests/ is modeled on the
    frontend rendering code, not on a recorded response.
    """
    rows = payload.get("data") or []
    notifications = []
    for row in rows:
        notifications.append({
            "reg_date": row.get("regDate"),
            "link": row.get("link"),
            "pledgors": row.get("pledgors") or [],
            "pledgees": row.get("pledgees") or [],
            "history": row.get("history") or [],
        })
    try:
        total = int(payload.get("total") or len(notifications))
    except (TypeError, ValueError):
        total = len(notifications)
    return {"total": total, "notifications": notifications}


def zalog_facts(result: Optional[dict],
                retrieved_at: Optional[str] = None) -> list[Fact]:
    """Fact contract: number of pledge notifications about the debtor."""
    retrieved_at = retrieved_at or utc_now_iso()
    total = result.get("total", 0) if result else 0
    return [Fact(field="zalog.count", value=total, source=SOURCE_NAME,
                 source_url=f"{BASE_URL}/search/index",
                 retrieved_at=retrieved_at)]


class ZalogiClient:
    """JSON client for /api/search/notary (pledgor search by INN/name).

    Degrades honestly: on 403 CAPTCHA raises ZalogiUnavailableError with
    the server message; the service records "zalogi: not_checked".
    """

    def __init__(self, config: dict, session: Optional[Any] = None,
                 captcha_token: Optional[str] = None):
        self.config = config
        interval = float(config.get("throttling", {}).get(
            "zalogi_min_interval_seconds", 5))
        self._limiter = RateLimiter(interval)
        self._session = session or make_session(config)
        self._session.headers.update({
            "Referer": f"{BASE_URL}/search/index",
            "Accept": "application/json, text/javascript, */*; q=0.01",
            "X-Requested-With": "XMLHttpRequest",
        })
        self._captcha_token = captcha_token

    @staticmethod
    def _check(resp: Any) -> dict:
        if resp.status_code == 403:
            reason = ""
            try:
                reason = json.loads(_decode_body(resp)).get("message", "")
            except ValueError:
                reason = _decode_body(resp)[:200]
            raise ZalogiUnavailableError(
                f"reestr-zalogov.ru HTTP 403: {reason or 'CAPTCHA'}")
        if resp.status_code >= 500:
            raise ZalogiUnavailableError(
                f"reestr-zalogov.ru HTTP {resp.status_code}: "
                f"{_decode_body(resp)[:200]}")
        resp.raise_for_status()
        try:
            return json.loads(_decode_body(resp))
        except ValueError:
            return {}

    def search_by_debtor(self, inn: Optional[str] = None,
                         name: Optional[str] = None) -> dict:
        """Search pledge notifications by pledgor (debtor) organization."""
        body: dict = {"isPledgorFilter": True,
                      "russianOrganization": {"name": name or "",
                                              "inn": inn or "",
                                              "ogrn": ""},
                      "limit": 10, "offset": 0}
        if self._captcha_token:
            body["captchaToken"] = self._captcha_token
        self._limiter.wait()
        resp = self._session.post(
            NOTARY_URL,
            data=json.dumps(body),
            headers={"Content-Type": "application/json;charset=utf-8"},
            timeout=http_timeout(self.config),
        )
        return parse_search_result(self._check(resp))

    def facts_for_debtor(self, inn: Optional[str] = None,
                         name: Optional[str] = None) -> list[Fact]:
        return zalog_facts(self.search_by_debtor(inn=inn, name=name))
