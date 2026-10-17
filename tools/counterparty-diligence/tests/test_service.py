"""Service pipeline test with stubbed adapters — no network."""

from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

import service  # noqa: E402
from sources.base import Fact  # noqa: E402


class _StubEgrul:
    def search(self, query, region=""):
        return [{"name": 'ООО "Ромашка"', "inn": "7707001234",
                 "ogrn": "1157746000000", "kpp": "770701001",
                 "registration_date": "2020-01-15"}]


class _StubEgrulEmpty:
    def search(self, query, region=""):
        return []


class _StubEgrulDown:
    def search(self, query, region=""):
        raise ConnectionError("kaput")


class _StubPb:
    def stoplist_flags(self, query):
        return ["mass_address"]


class _StubPbDown:
    def stoplist_flags(self, query):
        raise TimeoutError("slow")


class _StubNpchk:
    def __init__(self, state="1"):
        self._state = state

    def check_inn(self, inn, kpp=""):
        return {"inn": inn, "state": self._state}


class _StubSanctions:
    def __init__(self, matches):
        self._matches = matches

    def screen(self, name):
        return self._matches


def _make_service(monkeypatch, tmp_path, egrul, pb):
    config = service.load_config()
    config["cache"] = {"db_path": str(tmp_path / "svc.sqlite"),
                       "facts_ttl_hours": 24}
    svc = service.DiligenceService.__new__(service.DiligenceService)
    svc.config = config
    svc.cache = service.Cache(tmp_path / "svc.sqlite")
    svc.egrul = egrul
    svc.pb = pb
    svc.npchk = _StubNpchk()
    svc.sanctions = _StubSanctions([])
    return svc


def test_check_happy_path_scores_red_flags(monkeypatch, tmp_path):
    svc = _make_service(monkeypatch, tmp_path, _StubEgrul(), _StubPb())
    result = svc.check(inn="7707001234")
    assert result["ok"]
    flags = {c["flag"] for c in result["scorecard"]["contributions"]}
    assert "mass_address" in flags and "npchk_bad" in flags
    assert result["scorecard"]["score"] == 50
    assert result["scorecard"]["verdict"] == "yellow"


def test_check_unidentified_company(monkeypatch, tmp_path):
    svc = _make_service(monkeypatch, tmp_path, _StubEgrulEmpty(), _StubPb())
    result = svc.check(name="несуществующее лицо")
    assert not result["ok"]
    assert result["error"] == "company not identified"


def test_check_degrades_when_sources_fail(monkeypatch, tmp_path):
    svc = _make_service(monkeypatch, tmp_path, _StubEgrul(), _StubPbDown())
    result = svc.check(inn="7707001234")
    assert result["ok"]  # pipeline survives a dead source (PRD FR9)
    assert any("pb_stoplists" in w for w in result["warnings"])
    assert result["scorecard"]["score"] == 75  # only npchk_bad counted


def test_check_requires_identifier(monkeypatch, tmp_path):
    svc = _make_service(monkeypatch, tmp_path, _StubEgrul(), _StubPb())
    try:
        svc.check()
        assert False, "expected ValueError"
    except ValueError:
        pass


def test_facts_cached_for_inn(monkeypatch, tmp_path):
    svc = _make_service(monkeypatch, tmp_path, _StubEgrul(), _StubPb())
    svc.check(inn="7707001234")
    cached = svc.cache.get_facts("7707001234")
    assert cached and any(f.field == "inn" for f in cached)
    svc.cache.close()


def test_npchk_state_2_is_clean(monkeypatch, tmp_path):
    svc = _make_service(monkeypatch, tmp_path, _StubEgrul(), _StubPb())
    svc.npchk = _StubNpchk(state="2")  # registered, date not requested
    result = svc.check(inn="7707001234")
    flags = {c["flag"] for c in result["scorecard"]["contributions"]}
    assert "npchk_bad" not in flags
    assert result["scorecard"]["score"] == 75  # only mass_address
