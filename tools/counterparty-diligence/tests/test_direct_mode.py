"""RU-focus direct mode: Russian sources never pick up proxy env by default,
and foreign sanctions screening is opt-in (disabled out of the box)."""

from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

import pytest  # noqa: E402

from sources.base import make_session  # noqa: E402
from sources.egrul import EgrulClient  # noqa: E402
from sources.pb_stoplists import PbClient  # noqa: E402
from sources.npchk import NpchkClient  # noqa: E402
from sources.sanctions import SanctionsScreener  # noqa: E402

_THROTTLE = {"egrul_min_interval_seconds": 0,
             "pb_min_interval_seconds": 0,
             "npchk_min_interval_seconds": 0,
             "http_timeout_seconds": 5}


@pytest.fixture
def proxy_env(monkeypatch):
    """Host sits behind a proxy/VPN: ambient env vars are set."""
    monkeypatch.setenv("HTTP_PROXY", "http://127.0.0.1:9999")
    monkeypatch.setenv("HTTPS_PROXY", "http://127.0.0.1:9999")
    monkeypatch.setenv("ALL_PROXY", "socks5://127.0.0.1:9999")


def test_make_session_direct_by_default(proxy_env):
    session = make_session({})
    assert session.trust_env is False


def test_make_session_proxy_only_when_explicitly_enabled(proxy_env):
    session = make_session({"proxy": {"enabled": True}})
    assert session.trust_env is True


def test_ru_clients_ignore_proxy_env(proxy_env):
    config = {"throttling": _THROTTLE}
    for cls in (EgrulClient, PbClient, NpchkClient):
        client = cls(config)
        assert client._session.trust_env is False, cls.__name__


def test_npchk_direct_no_proxy_override(proxy_env):
    config = {"throttling": _THROTTLE,
              "npchk": {"direct_no_proxy": True, "verify_ssl": True}}
    client = NpchkClient(config)
    assert client._session.trust_env is False


class _ExplodingCache:
    """Any cache access is a bug while foreign screening is disabled."""

    def __getattr__(self, item):
        raise AssertionError(f"cache touched: {item}")


def _config_disabled():
    return {"throttling": _THROTTLE,
            "sanctions": {"foreign": {"enabled": False}}}


def test_foreign_disabled_screen_never_touches_cache(proxy_env):
    screener = SanctionsScreener(_config_disabled(), _ExplodingCache())
    assert screener.screen("ROSNEFT") == []


def test_foreign_disabled_status_note():
    screener = SanctionsScreener(_config_disabled(), _ExplodingCache())
    status = screener.status()
    assert status["enabled"] is False
    assert status["note"] == "disabled (ru-focus)"


def test_foreign_disabled_refresh_refuses():
    screener = SanctionsScreener(_config_disabled(), _ExplodingCache())
    result = screener.refresh_list("ofac_sdn")
    assert result["error"] == "disabled (ru-focus)"


def test_foreign_enabled_uses_cache(tmp_path, proxy_env):
    from cache import Cache
    from sources.sanctions import normalize_name, parse_ofac_sdn
    cache = Cache(tmp_path / "t.sqlite")
    version, entries = parse_ofac_sdn(
        (Path(__file__).parent / "fixtures" / "ofac_sdn.xml").read_text("utf-8"))
    cache.replace_sanctions_list("ofac_sdn", [
        {"alias": e["alias"], "normalized_name": normalize_name(e["alias"]),
         "raw": e["raw"]} for e in entries], version)
    config = {"throttling": _THROTTLE,
              "sanctions": {"foreign": {"enabled": True}}}
    screener = SanctionsScreener(config, cache)
    assert screener.screen("IVANOV IVAN")
    cache.close()
