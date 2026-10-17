import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

import dossier  # noqa: E402
from sources.base import Fact  # noqa: E402

DATA = {
    "company": {"name": "ООО \"Ромашка\"", "inn": "7707001234",
                "ogrn": "1157746000000"},
    "facts": [
        Fact(field="name", value="ООО \"Ромашка\"", source="egrul",
             retrieved_at="2026-10-17T00:00:00+00:00").to_dict(),
        Fact(field="inn", value="7707001234", source="egrul",
             retrieved_at="2026-10-17T00:00:00+00:00").to_dict(),
        Fact(field="registration_date", value="2020-01-15", source="egrul",
             retrieved_at="2026-10-17T00:00:00+00:00").to_dict(),
        Fact(field="management", value="Директор Иванов И.И.", source="egrul",
             retrieved_at="2026-10-17T00:00:00+00:00").to_dict(),
        Fact(field="npchk.state", value="2", source="npchk",
             retrieved_at="2026-10-17T00:00:00+00:00").to_dict(),
    ],
    "sanctions_matches": [],
    "scorecard": {
        "score": 65, "verdict": "yellow",
        "contributions": [{"flag": "npchk_bad", "weight": -25,
                           "source": "npchk",
                           "retrieved_at": "2026-10-17T00:00:00+00:00"}],
        "scoring_model_version": "v1",
    },
}


def test_markdown_has_all_sections_coverage_disclaimer():
    md = dossier.render(DATA, "md")
    for section in ("Сводка", "Регистрация", "Владение и управление", "МСП",
                    "Финансы", "Банкротство", "Залоги", "Суды",
                    "Репутация", "Цифровой след", "Красные флаги", "Вывод"):
        assert f"## {section}" in md
    assert "Покрытие: 6/12 блоков" in md
    assert "не является юридическим заключением" in md
    assert "v1" in md and "M2" in md
    assert "7707001234" in md
    assert "npchk_bad" in md


def test_json_format_returns_parseable_dict():
    raw = dossier.render(DATA, "json")
    parsed = json.loads(raw)
    assert parsed["coverage"] == "6/12"
    assert parsed["module_version"] == "M2"
    assert parsed["disclaimer"]
    assert set(parsed["sections"]) == set(dossier.SECTION_KEYS)


def test_degenerate_input_still_renders():
    md = dossier.render({"company": {}, "facts": [],
                         "scorecard": {"contributions": []}}, "md")
    assert "Покрытие" in md
    assert "—" in md  # placeholder dashes for missing facts
