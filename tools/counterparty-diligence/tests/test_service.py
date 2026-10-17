"""Service pipeline test with stubbed adapters — no network."""

from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

import service  # noqa: E402
from sources.base import Fact  # noqa: E402
from sources.zalogi import ZalogiUnavailableError  # noqa: E402


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


class _StubRmsp:
    def __init__(self, in_registry=True, active=True):
        self._in_registry = in_registry
        self._active = active

    def facts_for_inn(self, inn):
        return [
            Fact(field="msp.in_registry", value=self._in_registry,
                 source="rmsp", retrieved_at="2026-10-18T00:00:00+00:00"),
            Fact(field="msp.category", value="Малое предприятие",
                 source="rmsp", retrieved_at="2026-10-18T00:00:00+00:00"),
            Fact(field="msp.active", value=self._active,
                 source="rmsp", retrieved_at="2026-10-18T00:00:00+00:00"),
        ]


class _StubFedresurs:
    def __init__(self, bankrupt):
        self._bankrupt = bankrupt

    def facts_for_inn(self, inn):
        return [
            Fact(field="bankruptcy.in_registry", value=True,
                 source="fedresurs", retrieved_at="2026-10-18T00:00:00+00:00"),
            Fact(field="bankruptcy.is_bankrupt", value=self._bankrupt,
                 source="fedresurs", retrieved_at="2026-10-18T00:00:00+00:00"),
        ]


class _StubZalogi:
    def __init__(self, count):
        self._count = count

    def facts_for_debtor(self, inn=None, name=None):
        return [Fact(field="zalog.count", value=self._count,
                     source="zalogi", retrieved_at="2026-10-18T00:00:00+00:00")]


class _StubDigitalSleuth:
    def __init__(self, age_years=6.0):
        self._age = age_years

    def collect(self, domain):
        return [
            Fact(field="domain.created_date", value="2020-09-23",
                 source="digitalsleuth",
                 retrieved_at="2026-10-18T00:00:00+00:00"),
            Fact(field="domain.age_years", value=self._age,
                 source="digitalsleuth",
                 retrieved_at="2026-10-18T00:00:00+00:00"),
            Fact(field="domain.subdomains_count", value=3,
                 source="digitalsleuth",
                 retrieved_at="2026-10-18T00:00:00+00:00"),
        ]


def _make_service(monkeypatch, tmp_path, egrul, pb, standard_stubs=True):
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
    if standard_stubs:
        svc.rmsp = _StubRmsp()
        svc.fedresurs = _StubFedresurs(bankrupt=False)
        svc.zalogi = _StubZalogi(count=0)
        svc.digitalsleuth = _StubDigitalSleuth()
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


# --- M2 standard level (rmsp, fedresurs, zalogi, digitalsleuth) ---

def test_quick_level_skips_m2_sources(monkeypatch, tmp_path):
    svc = _make_service(monkeypatch, tmp_path, _StubEgrul(), _StubPb())
    result = svc.check(inn="7707001234", level="quick")
    assert result["ok"]
    assert result["msp"] == {} and result["bankruptcy"] == {}
    assert result["zalog"] == {} and result["digital"] == {}
    flags = {c["flag"] for c in result["scorecard"]["contributions"]}
    assert "msp_confirmed" not in flags


def test_standard_level_gathers_all_m2_blocks(monkeypatch, tmp_path):
    svc = _make_service(monkeypatch, tmp_path, _StubEgrul(), _StubPb())
    result = svc.check(inn="7707001234", level="standard",
                       domain="example.ru")
    assert result["ok"]
    assert result["msp"]["msp.in_registry"] is True
    assert result["bankruptcy"]["bankruptcy.is_bankrupt"] is False
    assert result["zalog"]["zalog.count"] == 0
    assert result["digital"]["domain.subdomains_count"] == 3
    facts = {f["field"] for f in result["facts"]}
    assert {"msp.in_registry", "bankruptcy.is_bankrupt", "zalog.count",
            "domain.created_date"} <= facts
    flags = {c["flag"] for c in result["scorecard"]["contributions"]}
    assert "msp_confirmed" in flags  # +5
    assert "bankruptcy" not in flags and "zalog_heavy" not in flags
    # 100 - 25 (mass_address) - 25 (npchk_bad) + 5 (msp_confirmed)
    assert result["scorecard"]["score"] == 55


def test_standard_flags_bankruptcy_zalog_heavy_young_domain(monkeypatch, tmp_path):
    svc = _make_service(monkeypatch, tmp_path, _StubEgrul(), _StubPb())
    svc.fedresurs = _StubFedresurs(bankrupt=True)
    svc.zalogi = _StubZalogi(count=8)            # > heavy_threshold 5
    svc.digitalsleuth = _StubDigitalSleuth(age_years=0.3)  # young domain
    result = svc.check(inn="7707001234", level="standard",
                       domain="fresh-example.ru")
    flags = {c["flag"] for c in result["scorecard"]["contributions"]}
    assert {"bankruptcy", "zalog_heavy", "young_domain", "msp_confirmed",
            "mass_address", "npchk_bad"} <= flags
    # 100 - 25 - 25 - 25 - 10 - 10 + 5 = 10
    assert result["scorecard"]["score"] == 10
    assert result["scorecard"]["verdict"] == "red"


def test_standard_degrades_when_m2_source_fails(monkeypatch, tmp_path):
    class _ZalogiDown:
        def facts_for_debtor(self, inn=None, name=None):
            raise ZalogiUnavailableError("reestr-zalogov.ru HTTP 403: CAPTCHA")

    svc = _make_service(monkeypatch, tmp_path, _StubEgrul(), _StubPb())
    svc.zalogi = _ZalogiDown()
    result = svc.check(inn="7707001234", level="standard")
    assert result["ok"]  # pipeline survives a CAPTCHA-gated source (FR9)
    assert any("zalogi: not_checked" in w for w in result["warnings"])
    assert "zalog.count" not in result["zalog"]
    flags = {c["flag"] for c in result["scorecard"]["contributions"]}
    assert "zalog_heavy" not in flags  # no guesses in place of facts


def test_standard_without_domain_skips_digital(monkeypatch, tmp_path):
    svc = _make_service(monkeypatch, tmp_path, _StubEgrul(), _StubPb())
    result = svc.check(inn="7707001234", level="standard")
    assert result["ok"]
    assert result["digital"] == {}
    flags = {c["flag"] for c in result["scorecard"]["contributions"]}
    assert "young_domain" not in flags
