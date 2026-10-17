"""Scoring model v1 (PRD §5). Weights and thresholds come from config.yaml."""

from __future__ import annotations

from typing import Optional

# Flag keys -> scoring class. Only M1-available red flags are wired:
# mass address/director (pb), npchk state != 0, sanctions match.
# Bankruptcy, disqualification and scale-gap flags arrive with M2 sources.
RED_FLAGS = ("mass_address", "mass_director", "unreliable_passport",
             "npchk_bad", "sanctions_match")
YELLOW_FLAGS = ("young_company", "name_changes")
PLUS_FLAGS = ("active_5y_plus", "revenue_growing", "msp_confirmed",
              "own_address", "long_registration")


def classify(score: int, thresholds: dict) -> str:
    if score >= thresholds.get("green", 70):
        return "green"
    if score >= thresholds.get("yellow", 40):
        return "yellow"
    return "red"


def score(flags: list[dict], config: dict) -> dict:
    """Score a list of flag dicts.

    Each flag: {"flag": key, "source": str, "retrieved_at": str, ...}.
    Returns the scorecard with per-flag contributions and the verdict.
    """
    scoring = config.get("scoring", {})
    red_w = scoring.get("red_weight", -25)
    yellow_w = scoring.get("yellow_weight", -10)
    plus_w = scoring.get("plus_weight", 5)
    plus_cap = scoring.get("plus_cap", 15)
    thresholds = scoring.get("thresholds", {"green": 70, "yellow": 40})

    contributions = []
    plus_total = 0
    for flag in flags:
        key = flag["flag"]
        if key in RED_FLAGS:
            weight = red_w
        elif key in YELLOW_FLAGS:
            weight = yellow_w
        elif key in PLUS_FLAGS:
            weight = plus_w
            if weight > 0 and plus_total + weight > plus_cap:
                continue  # plus side is capped
            plus_total += max(weight, 0)
        else:
            continue  # unknown flag: no contribution, do not guess
        contributions.append({
            "flag": key,
            "weight": weight,
            "source": flag.get("source", ""),
            "retrieved_at": flag.get("retrieved_at", ""),
        })

    raw = 100 + sum(c["weight"] for c in contributions)
    total = max(0, raw)
    return {
        "score": total,
        "verdict": classify(total, thresholds),
        "contributions": contributions,
        "scoring_model_version": scoring.get("model_version", "v1"),
    }
