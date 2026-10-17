"""Dossier rendering: markdown (SKILL.md v1 structure) or raw JSON."""

from __future__ import annotations

import json
from typing import Any, Optional

MODULE_VERSION = "M1"

DISCLAIMER = ("Дисклеймер: данный отчёт носит информационный характер "
              "и не является юридическим заключением.")

# Sections whose presence counts toward coverage (M1 sources only).
SECTION_KEYS = ["summary", "registration", "ownership", "finances",
                "courts", "reputation", "red_flags", "conclusion"]


def _section_body(key: str, data: dict) -> Optional[str]:
    company = data.get("company") or {}
    facts = {f["field"]: f["value"] for f in data.get("facts", [])}
    flags = data.get("scorecard", {}).get("contributions", [])
    lines = {
        "summary": lambda: [
            f"- Наименование: {company.get('name') or facts.get('name', '—')}",
            f"- ИНН: {company.get('inn') or facts.get('inn', '—')}",
            f"- ОГРН: {company.get('ogrn') or facts.get('ogrn', '—')}",
            f"- Балл: {data.get('scorecard', {}).get('score', '—')} "
            f"({data.get('scorecard', {}).get('verdict', '—')})",
        ],
        "registration": lambda: [
            f"- Дата регистрации: {facts.get('registration_date', '—')}",
            f"- Адрес: {facts.get('address', '—')}",
            f"- Статус: {facts.get('status', '—')}",
            f"- ОКВЭД: {facts.get('okved', '—')}",
        ],
        "ownership": lambda: [
            f"- Руководитель: {facts.get('management', '—')}",
        ],
        "finances": lambda: ["- БФО (bo.nalog.gov.ru): не подключено в M1."],
        "courts": lambda: [
            "- Картотека арбитражных дел (kad.arbitr.ru): не подключена в M1 "
            "(антибот; best-effort планируется в M3).",
            "- ФССП: исключён (капча, API отключён с 2022).",
        ],
        "reputation": lambda: [
            f"- Статус ФНС недобросовестных поставщиков: "
            f"{facts.get('npchk.state', '—')}",
            f"- Санкционный скрининг: "
            f"{'совпадений нет' if not data.get('sanctions_matches') else data['sanctions_matches']}",
        ],
        "red_flags": lambda: [
            f"- {c['flag']}: {c['weight']} ({c['source']}, {c['retrieved_at']})"
            for c in flags
        ] or ["- Не выявлены."],
        "conclusion": lambda: [
            f"- Итоговая оценка: {data.get('scorecard', {}).get('score', '—')}/100 "
            f"— {data.get('scorecard', {}).get('verdict', '—')}.",
            f"- Покрытие: {data.get('coverage', '?')}.",
            f"- Модуль: {MODULE_VERSION}, скоринг: "
            f"{data.get('scorecard', {}).get('scoring_model_version', 'v1')}.",
        ],
    }
    return "\n".join(lines[key]())


def build_dossier(data: dict) -> dict:
    """Assemble the structured dossier with coverage accounting."""
    sections = {}
    for key in SECTION_KEYS:
        body = _section_body(key, data)
        sections[key] = body
    filled = sum(1 for key in ("summary", "registration", "ownership",
                               "reputation", "red_flags", "conclusion")
                 if sections[key])
    coverage = f"{filled}/{len(SECTION_KEYS)}"
    return {"sections": sections, "coverage": coverage,
            "disclaimer": DISCLAIMER, "module_version": MODULE_VERSION,
            "scoring_model_version":
                data.get("scorecard", {}).get("scoring_model_version", "v1")}


def render_markdown(data: dict) -> str:
    dossier = build_dossier(data)
    titles = {
        "summary": "Сводка", "registration": "Регистрация",
        "ownership": "Владение и управление", "finances": "Финансы (БФО)",
        "courts": "Суды и исполнительные производства",
        "reputation": "Репутация и реестры", "red_flags": "Красные флаги",
        "conclusion": "Вывод",
    }
    parts = [f"# Досье контрагента\n",
             f"Покрытие: {dossier['coverage']} блоков\n"]
    for key in SECTION_KEYS:
        parts.append(f"## {titles[key]}\n\n{dossier['sections'][key]}\n")
    parts.append(f"---\n{dossier['disclaimer']}\n")
    return "\n".join(parts)


def render(data: dict, fmt: str = "md") -> str:
    if fmt == "json":
        return json.dumps(build_dossier(data), ensure_ascii=False, indent=2)
    return render_markdown(data)
