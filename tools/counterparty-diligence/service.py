"""Orchestration: resolve a counterparty, gather facts, score, build dossier.

Wires the adapters (M1: egrul, pb stop-lists, npchk, sanctions; M2: rmsp,
fedresurs, zalogi, digitalsleuth) with the cache and the scoring model.
Degrades to "not checked" when a source fails (PRD FR9): the pipeline
never falls over because one registry is down.

Levels (PRD FR3): quick = M1 sources; standard = quick + SME registry,
Fedresurs bankruptcy, FNP pledges, digital footprint (domain if passed);
full = standard + cross-checks (M3).
"""

from __future__ import annotations

from datetime import datetime
from pathlib import Path
from typing import Optional

import yaml

from cache import Cache
from scoring import score as run_scoring
from dossier import render, MODULE_VERSION
from sources.base import Fact, utc_now_iso
from sources.egrul import EgrulClient, company_facts
from sources.pb_stoplists import PbClient
from sources.npchk import NpchkClient, OK_STATES
from sources.sanctions import SanctionsScreener
from sources.rmsp import RmspClient
from sources.fedresurs import FedresursClient
from sources.zalogi import ZalogiClient
from sources.digitalsleuth import DigitalSleuthClient, age_years

BASE_DIR = Path(__file__).parent

NOT_CHECKED = "not_checked"

QUICK_LEVEL_SOURCES = ("pb_stoplists", "npchk", "sanctions")
STANDARD_EXTRA_SOURCES = ("rmsp", "fedresurs", "zalogi", "digitalsleuth")


def load_config(path: Optional[Path] = None) -> dict:
    path = path or (BASE_DIR / "config.yaml")
    with open(path, "r", encoding="utf-8") as fh:
        return yaml.safe_load(fh)


class DiligenceService:
    def __init__(self, config: Optional[dict] = None, cache: Optional[Cache] = None):
        self.config = config or load_config()
        db_path = BASE_DIR / self.config.get("cache", {}).get(
            "db_path", "data/dd-cache.sqlite")
        self.cache = cache or Cache(db_path)
        self.egrul = EgrulClient(self.config)
        self.pb = PbClient(self.config)
        self.npchk = NpchkClient(self.config)
        self.sanctions = SanctionsScreener(self.config, self.cache)
        self.rmsp = RmspClient(self.config)
        self.fedresurs = FedresursClient(self.config)
        self.zalogi = ZalogiClient(self.config)
        self.digitalsleuth = DigitalSleuthClient(self.config)

    # --- fact gathering ---

    def _egrul_company(self, inn: Optional[str], name: Optional[str]) -> tuple[Optional[dict], list[Fact], list[str]]:
        warnings = []
        query = inn or name
        try:
            companies = self.egrul.search(query) if query else []
        except Exception as exc:
            warnings.append(f"egrul: {NOT_CHECKED} ({exc})")
            return None, [], warnings
        if not companies:
            warnings.append(f"egrul: no match for {query!r}")
            return None, [], warnings
        company = companies[0]
        if len(companies) > 1 and not inn:
            warnings.append("egrul: multiple matches; first taken — refine by INN")
        return company, company_facts(company), warnings

    def _pb_flags(self, company: dict) -> tuple[list[str], list[str]]:
        warnings = []
        query = company.get("inn") or company.get("name") or ""
        try:
            return self.pb.stoplist_flags(query), warnings
        except Exception as exc:
            warnings.append(f"pb_stoplists: {NOT_CHECKED} ({exc})")
            return [], warnings

    def _npchk_record(self, company: dict) -> tuple[Optional[dict], list[str]]:
        warnings = []
        inn = company.get("inn")
        if not inn:
            return None, warnings
        try:
            return self.npchk.check_inn(inn, company.get("kpp", "")), warnings
        except Exception as exc:
            warnings.append(f"npchk: {NOT_CHECKED} ({exc})")
            return None, warnings

    def _sanctions_matches(self, company: dict) -> tuple[list[dict], list[str]]:
        warnings = []
        name = company.get("name")
        if not name:
            return [], warnings
        try:
            return self.sanctions.screen(name), warnings
        except Exception as exc:
            warnings.append(f"sanctions: {NOT_CHECKED} ({exc})")
            return [], warnings

    # --- M2 fact gathering (standard level) ---

    def _msp_facts(self, company: dict) -> tuple[list[Fact], list[str]]:
        warnings = []
        inn = company.get("inn")
        if not inn:
            return [], warnings
        try:
            return self.rmsp.facts_for_inn(inn), warnings
        except Exception as exc:
            warnings.append(f"rmsp: {NOT_CHECKED} ({exc})")
            return [], warnings

    def _bankruptcy_facts(self, company: dict) -> tuple[list[Fact], list[str]]:
        warnings = []
        inn = company.get("inn")
        if not inn:
            return [], warnings
        try:
            return self.fedresurs.facts_for_inn(inn), warnings
        except Exception as exc:
            warnings.append(f"fedresurs: {NOT_CHECKED} ({exc})")
            return [], warnings

    def _zalog_facts(self, company: dict) -> tuple[list[Fact], list[str]]:
        warnings = []
        inn = company.get("inn")
        name = company.get("name")
        if not inn and not name:
            return [], warnings
        try:
            return self.zalogi.facts_for_debtor(inn=inn, name=name), warnings
        except Exception as exc:
            warnings.append(f"zalogi: {NOT_CHECKED} ({exc})")
            return [], warnings

    def _digital_facts(self, domain: Optional[str]) -> tuple[list[Fact], list[str]]:
        warnings = []
        if not domain:
            return [], warnings
        try:
            return self.digitalsleuth.collect(domain), warnings
        except Exception as exc:
            warnings.append(f"digitalsleuth: {NOT_CHECKED} ({exc})")
            return [], warnings

    @staticmethod
    def _fact_map(facts: list[Fact]) -> dict:
        return {f.field: f.value for f in facts}

    @staticmethod
    def _company_age_years(company: dict) -> Optional[float]:
        raw = company.get("registration_date") or ""
        try:
            registered = datetime.strptime(raw, "%d.%m.%Y").date()
        except ValueError:
            return None
        return age_years(registered)

    # --- public API used by MCP tools ---

    def check(self, inn: Optional[str] = None, name: Optional[str] = None,
              level: str = "quick", domain: Optional[str] = None) -> dict:
        if not inn and not name:
            raise ValueError("either inn or name is required")
        if level not in ("quick", "standard", "full"):
            raise ValueError("level must be quick|standard|full")

        warnings: list[str] = []
        company, facts, w = self._egrul_company(inn, name)
        warnings += w
        if company is None:
            return {"ok": False, "error": "company not identified",
                    "warnings": warnings}
        inn = company.get("inn") or inn

        # Cache short-circuit for registry facts (PRD FR10).
        ttl = float(self.config.get("cache", {}).get("facts_ttl_hours", 24))
        cached = self.cache.get_facts(inn, ttl_hours=ttl) if inn else None
        if cached is not None:
            facts = cached

        pb_flags, w = self._pb_flags(company)
        warnings += w
        npchk_record, w = self._npchk_record(company)
        warnings += w
        sanctions_matches, w = self._sanctions_matches(company)
        warnings += w

        # M2 sources: standard and full levels only (PRD FR3).
        msp_facts: list[Fact] = []
        bankruptcy_facts: list[Fact] = []
        zalog_facts: list[Fact] = []
        digital_facts: list[Fact] = []
        if level in ("standard", "full"):
            msp_facts, w = self._msp_facts(company)
            warnings += w
            bankruptcy_facts, w = self._bankruptcy_facts(company)
            warnings += w
            zalog_facts, w = self._zalog_facts(company)
            warnings += w
            digital_facts, w = self._digital_facts(domain)
            warnings += w
        facts += msp_facts + bankruptcy_facts + zalog_facts + digital_facts

        retrieved_at = utc_now_iso()
        flags = [
            {"flag": flag, "source": "pb_stoplists", "retrieved_at": retrieved_at}
            for flag in pb_flags
        ]
        if npchk_record and npchk_record["state"] not in OK_STATES:
            flags.append({"flag": "npchk_bad", "source": "npchk",
                          "retrieved_at": retrieved_at})
        if sanctions_matches:
            flags.append({"flag": "sanctions_match", "source": "sanctions",
                          "retrieved_at": retrieved_at})

        msp = self._fact_map(msp_facts)
        bankruptcy = self._fact_map(bankruptcy_facts)
        zalog = self._fact_map(zalog_facts)
        digital = self._fact_map(digital_facts)
        if msp.get("msp.in_registry") is True and msp.get("msp.active", True):
            flags.append({"flag": "msp_confirmed", "source": "rmsp",
                          "retrieved_at": retrieved_at})
        if bankruptcy.get("bankruptcy.is_bankrupt") is True:
            flags.append({"flag": "bankruptcy", "source": "fedresurs",
                          "retrieved_at": retrieved_at})
        zalog_count = zalog.get("zalog.count")
        threshold = int(self.config.get("zalogi", {}).get("heavy_threshold", 5))
        if isinstance(zalog_count, int) and zalog_count > threshold:
            flags.append({"flag": "zalog_heavy", "source": "zalogi",
                          "retrieved_at": retrieved_at})
        domain_age = digital.get("domain.age_years")
        if isinstance(domain_age, (int, float)):
            young = float(self.config.get("domain", {}).get("young_age_years", 1))
            mature = float(self.config.get("domain", {}).get(
                "mature_company_years", 3))
            company_age = self._company_age_years(company)
            if domain_age < young and (company_age is None or company_age >= mature):
                flags.append({"flag": "young_domain", "source": "digitalsleuth",
                              "retrieved_at": retrieved_at})

        scorecard = run_scoring(flags, self.config)
        result_facts = [f.to_dict() for f in facts]
        if cached is None and inn:
            self.cache.set_facts(inn, facts)

        return {
            "ok": True,
            "inn": inn,
            "company": {k: company.get(k) for k in
                        ("name", "inn", "ogrn", "kpp", "address", "okved",
                         "management", "registration_date", "status")},
            "facts": result_facts,
            "pb_flags": pb_flags,
            "npchk": npchk_record,
            "sanctions_matches": sanctions_matches,
            "msp": msp,
            "bankruptcy": bankruptcy,
            "zalog": zalog,
            "digital": digital,
            "scorecard": scorecard,
            "level": level,
            "module_version": MODULE_VERSION,
            "warnings": warnings,
        }

    def dossier(self, inn: str, fmt: str = "md", domain: Optional[str] = None) -> str:
        result = self.check(inn=inn, level="standard", domain=domain)
        if not result.get("ok"):
            raise ValueError(f"cannot build dossier: {result.get('error')}")
        result["coverage_note"] = None
        return render(result, fmt=fmt)

    def sources_status(self) -> dict:
        """Availability snapshot: sanctions lists report cached versions;
        live sources are probed lazily on first use (egrul throttles >=30s)."""
        status = {"sanctions_lists": self.sanctions.status()}
        for source in ("egrul", "pb_stoplists", "npchk", "rmsp", "fedresurs",
                       "zalogi", "digitalsleuth"):
            status[source] = {"reachable": None, "note": "probed on first use"}
        status["zalogi"]["note"] = ("CAPTCHA-gated at last live check "
                                    "2026-10-18; degrades to not_checked")
        return status
