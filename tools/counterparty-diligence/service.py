"""Orchestration: resolve a counterparty, gather facts, score, build dossier.

Wires the M1 adapters (egrul, pb stop-lists, npchk, sanctions) with the
cache and the scoring model. Degrades to "not checked" when a source fails
(PRD FR9): the pipeline never falls over because one registry is down.
"""

from __future__ import annotations

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

BASE_DIR = Path(__file__).parent

NOT_CHECKED = "not_checked"


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

    # --- public API used by MCP tools ---

    def check(self, inn: Optional[str] = None, name: Optional[str] = None,
              level: str = "quick") -> dict:
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
            "scorecard": scorecard,
            "level": level,
            "module_version": MODULE_VERSION,
            "warnings": warnings,
        }

    def dossier(self, inn: str, fmt: str = "md") -> str:
        result = self.check(inn=inn, level="quick")
        if not result.get("ok"):
            raise ValueError(f"cannot build dossier: {result.get('error')}")
        result["coverage_note"] = None
        return render(result, fmt=fmt)

    def sources_status(self) -> dict:
        """Availability snapshot: sanctions lists report cached versions;
        live sources are probed lazily on first use (egrul throttles >=30s)."""
        status = {"sanctions_lists": self.sanctions.status()}
        for source in ("egrul", "pb_stoplists", "npchk"):
            status[source] = {"reachable": None, "note": "probed on first use"}
        return status
