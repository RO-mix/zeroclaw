from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

import scoring  # noqa: E402

CONFIG = {
    "scoring": {
        "model_version": "v1",
        "red_weight": -25,
        "yellow_weight": -10,
        "plus_weight": 5,
        "plus_cap": 15,
        "thresholds": {"green": 70, "yellow": 40},
    }
}


def _flag(key):
    return {"flag": key, "source": "test", "retrieved_at": "2026-10-17T00:00:00+00:00"}


def test_clean_company_is_green_100():
    card = scoring.score([_flag("own_address"), _flag("long_registration")], CONFIG)
    assert card["score"] == 110  # max(0, 100+sum) per formula; only the plus side is capped
    assert card["verdict"] == "green"
    assert card["scoring_model_version"] == "v1"


def test_single_red_flag():
    card = scoring.score([_flag("mass_address")], CONFIG)
    assert card["score"] == 75
    assert card["verdict"] == "green"  # 75 >= 70


def test_red_and_yellow_drop_to_yellow():
    card = scoring.score([_flag("mass_address"), _flag("npchk_bad"),
                          _flag("young_company")], CONFIG)
    assert card["score"] == 100 - 25 - 25 - 10
    assert card["verdict"] == "yellow"


def test_multiple_reds_floor_at_zero():
    flags = [_flag("mass_address"), _flag("mass_director"),
             _flag("sanctions_match"), _flag("npchk_bad"),
             _flag("unreliable_passport")]
    card = scoring.score(flags, CONFIG)
    assert card["score"] == 0
    assert card["verdict"] == "red"


def test_plus_side_is_capped():
    flags = [_flag(f) for f in ("active_5y_plus", "revenue_growing",
                                "msp_confirmed", "own_address",
                                "long_registration")]
    card = scoring.score(flags, CONFIG)
    plus = sum(c["weight"] for c in card["contributions"] if c["weight"] > 0)
    assert plus == 15  # 5 x +5 would be 25 without the cap


def test_each_contribution_has_audit_fields():
    card = scoring.score([_flag("sanctions_match")], CONFIG)
    (c,) = card["contributions"]
    assert c["flag"] == "sanctions_match"
    assert c["weight"] == -25
    assert c["source"] == "test"
    assert c["retrieved_at"]


def test_unknown_flag_ignored():
    card = scoring.score([_flag("bankruptcy")], CONFIG)  # M2 flag, not wired
    assert card["score"] == 100
    assert card["contributions"] == []
