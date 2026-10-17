"""Digital footprint adapter: domain age (whois/RDAP) + subdomains (crt.sh).

Scope: the domain is passed explicitly (check(domain=...)); extracting it
from user materials is the legalplat pipeline's job, not this adapter.

Sources:
- whois.tcinet.ru:43 (RU-CENTER) for .ru/.рф/.su — raw socket, no deps.
  Verified live 2026-10-18: `created: 1997-09-23T09:45:07Z` parsed for
  yandex.ru.
- rdap.org for gTLD and other TLDs without a reachable whois server
  (redirects to the registry RDAP; events[].eventAction == "registration").
- crt.sh certificate transparency: GET https://crt.sh/?q=%.<domain>&output
  =json -> JSON array with name_value (one or more SANs per row). Known
  flakiness (verified live 2026-10-18: HTTP 502 then read timeout from
  the RU network) — the client retries once and raises CrtShUnavailableError;
  the pipeline degrades the subdomain count to "not checked".

Scoring input (service.py): domain age vs company age -> young_domain flag.
"""

from __future__ import annotations

import json
import socket
from datetime import date, datetime, timezone
from typing import Any, Optional

from .base import Fact, RateLimiter, http_timeout, make_session, utc_now_iso

SOURCE_NAME = "digitalsleuth"
CRT_SH_URL = "https://crt.sh/"
RDAP_URL = "https://rdap.org/domain/"
RU_WHOIS_HOST = "whois.tcinet.ru"
RU_WHOIS_PORT = 43
RU_TLDS = ("ru", "su", "рф")


class CrtShUnavailableError(RuntimeError):
    """crt.sh failed after the configured retries (502/timeouts are common)."""


def _decode_body(resp: Any) -> str:
    try:
        return resp.content.decode("utf-8")
    except UnicodeDecodeError:
        return resp.content.decode("cp1251")


def parse_whois(text: str) -> dict:
    """Parse the whois.tcinet.ru text response (creation/expiry/registrar)."""
    info: dict[str, str] = {}
    for line in text.splitlines():
        if ":" not in line:
            continue
        key, _, value = line.partition(":")
        key = key.strip().lower()
        value = value.strip()
        if key in ("created", "paid-till", "registrar", "registrar name",
                   "source") and value:
            info[key] = value
    return info


def parse_whois_created(info: dict) -> Optional[date]:
    """Extract the creation date from parsed whois fields (several formats)."""
    raw = info.get("created") or ""
    return _parse_any_date(raw)


def _parse_any_date(raw: str) -> Optional[date]:
    raw = (raw or "").strip()
    if not raw:
        return None
    for fmt in ("%Y-%m-%dT%H:%M:%SZ", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d",
                "%d.%m.%Y", "%Y.%m.%d", "%Y/%m/%d"):
        try:
            return datetime.strptime(raw, fmt).date()
        except ValueError:
            continue
    # ISO with timezone, e.g. 1997-09-23T09:45:07Z
    try:
        return datetime.fromisoformat(raw.replace("Z", "+00:00")).date()
    except ValueError:
        return None


def whois_query(domain: str, host: str = RU_WHOIS_HOST,
                port: int = RU_WHOIS_PORT, timeout: float = 20) -> str:
    """Raw-socket whois query; returns the decoded server response."""
    with socket.create_connection((host, port), timeout=timeout) as sock:
        sock.sendall(f"{domain}\r\n".encode("idna"))
        chunks = []
        while True:
            data = sock.recv(4096)
            if not data:
                break
            chunks.append(data)
    return b"".join(chunks).decode("utf-8", "replace")


def parse_crt_sh(payload: Any) -> list[str]:
    """Unique subdomains from the crt.sh JSON array."""
    names: set[str] = set()
    for row in payload or []:
        name_value = row.get("name_value") or ""
        for name in name_value.splitlines():
            name = name.strip().lower().lstrip("*.")
            if name:
                names.add(name)
    return sorted(names)


def parse_rdap_created(payload: dict) -> Optional[date]:
    for event in payload.get("events") or []:
        if str(event.get("eventAction", "")).lower() in (
                "registration", "registered", "created"):
            return _parse_any_date(event.get("eventDate", ""))
    return None


def age_years(since: Optional[date], now: Optional[date] = None) -> Optional[float]:
    if since is None:
        return None
    now = now or datetime.now(timezone.utc).date()
    return round((now - since).days / 365.25, 1)


def domain_facts(domain: str, created: Optional[date],
                 registrar: Optional[str], subdomains: Optional[list[str]],
                 source_url: str, retrieved_at: Optional[str] = None,
                 not_checked: Optional[str] = None) -> list[Fact]:
    """Fact contract for the digital footprint check.

    not_checked: source name when a sub-source failed (recorded honestly
    instead of guessing, PRD FR9).
    """
    retrieved_at = retrieved_at or utc_now_iso()
    facts = []
    if created is not None:
        facts.append(Fact(field="domain.created_date",
                          value=created.isoformat(), source=SOURCE_NAME,
                          source_url=source_url, retrieved_at=retrieved_at))
        age = age_years(created)
        facts.append(Fact(field="domain.age_years", value=age,
                          source=SOURCE_NAME, source_url=source_url,
                          retrieved_at=retrieved_at))
    elif not_checked:
        facts.append(Fact(field="domain.created_date",
                          value=f"not_checked ({not_checked})",
                          source=SOURCE_NAME, source_url=source_url,
                          retrieved_at=retrieved_at,
                          confidence="unverified"))
    if registrar:
        facts.append(Fact(field="domain.registrar", value=registrar,
                          source=SOURCE_NAME, source_url=source_url,
                          retrieved_at=retrieved_at))
    if subdomains is not None:
        facts.append(Fact(field="domain.subdomains_count", value=len(subdomains),
                          source=SOURCE_NAME, source_url=CRT_SH_URL,
                          retrieved_at=retrieved_at))
    elif not_checked:
        facts.append(Fact(field="domain.subdomains_count",
                          value=f"not_checked ({not_checked})",
                          source=SOURCE_NAME, source_url=CRT_SH_URL,
                          retrieved_at=retrieved_at,
                          confidence="unverified"))
    return facts


class DigitalSleuthClient:
    """crt.sh + whois/RDAP client; sub-sources fail independently."""

    def __init__(self, config: dict, session: Optional[Any] = None):
        self.config = config
        interval = float(config.get("throttling", {}).get(
            "digitalsleuth_min_interval_seconds", 5))
        self._limiter = RateLimiter(interval)
        self._session = session or make_session(config)
        self._whois_timeout = float(config.get("digitalsleuth", {}).get(
            "whois_timeout_seconds", 20))
        self._crt_retries = int(config.get("digitalsleuth", {}).get(
            "crtsh_retries", 1))

    # --- crt.sh ---

    def crt_sh_subdomains(self, domain: str) -> list[str]:
        domain = domain.strip().lower().lstrip(".")
        last_error: Optional[Exception] = None
        for _ in range(self._crt_retries + 1):
            try:
                self._limiter.wait()
                resp = self._session.get(
                    CRT_SH_URL,
                    params={"q": f"%.{domain}", "output": "json"},
                    timeout=http_timeout(self.config),
                )
                if resp.status_code != 200:
                    last_error = CrtShUnavailableError(
                        f"crt.sh HTTP {resp.status_code}")
                    continue
                return parse_crt_sh(json.loads(_decode_body(resp)))
            except (ValueError, OSError) as exc:
                last_error = exc
        raise CrtShUnavailableError(f"crt.sh failed: {last_error}")

    # --- whois / RDAP ---

    def _whois_created(self, domain: str) -> tuple[Optional[date], Optional[str]]:
        tld = domain.rsplit(".", 1)[-1]
        if tld in RU_TLDS:
            text = whois_query(domain, timeout=self._whois_timeout)
            info = parse_whois(text)
            return parse_whois_created(info), info.get("registrar")
        return self._rdap_created(domain)

    def _rdap_created(self, domain: str) -> tuple[Optional[date], Optional[str]]:
        self._limiter.wait()
        resp = self._session.get(f"{RDAP_URL}{domain}",
                                 timeout=http_timeout(self.config))
        resp.raise_for_status()
        payload = json.loads(_decode_body(resp))
        registrar = None
        for entity in payload.get("entities") or []:
            if "registrar" in (entity.get("roles") or []):
                registrar = entity.get("handle")
                break
        return parse_rdap_created(payload), registrar

    # --- combined ---

    def collect(self, domain: str) -> list[Fact]:
        """Facts for one domain; each sub-source degrades independently."""
        domain = (domain or "").strip().lower()
        retrieved_at = utc_now_iso()
        created: Optional[date] = None
        registrar: Optional[str] = None
        failed: list[str] = []
        try:
            created, registrar = self._whois_created(domain)
        except OSError as exc:
            failed.append(f"whois/rdap: {exc}")
        subdomains: Optional[list[str]] = None
        try:
            subdomains = self.crt_sh_subdomains(domain)
        except CrtShUnavailableError as exc:
            failed.append(str(exc))
        facts = domain_facts(domain, created, registrar, subdomains,
                             f"{RDAP_URL}{domain}" if not domain.endswith(RU_TLDS)
                             else f"whois://{RU_WHOIS_HOST}/{domain}",
                             retrieved_at=retrieved_at,
                             not_checked="; ".join(failed) or None)
        return facts
