"""Shared contracts: facts, rate limiting, HTTP helpers."""

from __future__ import annotations

import threading
import time
from dataclasses import dataclass, field, asdict
from typing import Any, Optional

import requests

CONF_CONFIRMED = "confirmed"
CONF_INDIRECT = "indirect"
CONF_UNVERIFIED = "unverified"


@dataclass
class Fact:
    """Single sourced fact (PRD FR2 contract)."""
    field: str
    value: Any
    source: str
    source_url: str = ""
    retrieved_at: str = ""
    confidence: str = CONF_CONFIRMED

    def to_dict(self) -> dict:
        return asdict(self)


class RateLimiter:
    """Enforce a minimum interval between requests to one host."""

    def __init__(self, min_interval_seconds: float):
        self._min = min_interval_seconds
        self._last = 0.0
        self._lock = threading.Lock()

    def wait(self) -> None:
        with self._lock:
            now = time.monotonic()
            delta = now - self._last
            if delta < self._min:
                time.sleep(self._min - delta)
            self._last = time.monotonic()


def make_session(config: dict) -> requests.Session:
    """Session factory. RU-focus policy: direct connections by default.

    Ambient HTTP(S)_PROXY env vars are ignored unless proxy.enabled is
    explicitly set to true in config.yaml. Verified symptom: FNS endpoints
    misbehave (HTTP 500) when the host's proxy/VPN env leaks into requests.
    """
    session = requests.Session()
    session.trust_env = bool(config.get("proxy", {}).get("enabled", False))
    return session


def http_timeout(config: dict) -> float:
    return float(config.get("throttling", {}).get("http_timeout_seconds", 30))


def utc_now_iso() -> str:
    from datetime import datetime, timezone
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()
