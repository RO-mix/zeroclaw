"""Dossier rendering: markdown (SKILL.md v1 structure) or raw JSON."""

from __future__ import annotations

import json
from typing import Any, Optional

MODULE_VERSION = "M2"

DISCLAIMER = ("Дисклеймер: данный отчёт носит информационный характер "
              "и не является юридическим заключением.")

# Sections whose presence counts toward coverage. M2 adds «МСП»,
# «Банкротство», «Залоги», «Цифровой след».
SECTION_KEYS = ["summary", "registration", "ownership", "msp", "finances",
                "bankruptcy", "zalog", "courts", "reputation", "digital",
                "red_flags", "conclusion"]

# A section counts as filled only when backed by a real fact; stubs
# («не подключено», «не проверено») do not raise coverage (PRD FR6/FR9).
FILLED_FACT = {
    "msp": "msp.in_registry",
    "bankruptcy": "bankruptcy.in_registry",
    "zalog": "zalog.count",
    "digital": "domain.created_date",
}
ALWAYS_FILLED = ("summary", "registration", "ownership", "reputation",
                 "red_flags", "conclusion")


def _fmt_msp(facts: dict) -> list[str]:
    if "msp.in_registry" not in facts:
        return ["- Реестр МСП (rmsp.nalog.ru): не проверено."]
    if facts["msp.in_registry"] is not True:
        return ["- Реестр МСП: субъект не включён в единый реестр МСП."]
    lines = [f"- Статус МСП: в реестре ({facts.get('msp.category', 'категория не указана')})."]
    if facts.get("msp.inclusion_date"):
        lines.append(f"- Дата включения в реестр: {facts['msp.inclusion_date']}.")
    return lines


def _fmt_bankruptcy(facts: dict) -> list[str]:
    if "bankruptcy.in_registry" not in facts:
        return ["- Федресурс (fedresurs.ru): не проверено."]
    if facts["bankruptcy.in_registry"] is not True:
        return ["- Федресурс: сообщений по компании нет (банкротные процедуры не выявлены)."]
    if facts.get("bankruptcy.is_bankrupt") is True:
        return [f"- КРАСНЫЙ ФЛАГ: по данным Федресурса компания в банкротной процедуре. "
                f"Статус: {facts.get('bankruptcy.status', '—')}.",
                f"- Карточка: {facts.get('bankruptcy.guid', '—')}."]
    return [f"- Федресурс: компания в базе, банкротные процедуры не выявлены "
            f"(статус: {facts.get('bankruptcy.status', '—')})."]


def _fmt_zalog(facts: dict) -> list[str]:
    if "zalog.count" not in facts:
        return ["- Реестр залогов ФНП (reestr-zalogov.ru): не проверено "
                "(источник с CAPTCHA — честная деградация, см. README)."]
    return [f"- Уведомлений о залоге движимого имущества по должнику: "
            f"{facts['zalog.count']}."]


def _fmt_digital(facts: dict) -> list[str]:
    if "domain.created_date" not in facts:
        return ["- Цифровой след: домен не передан (check(domain=...)) или не проверено."]
    created = facts.get("domain.created_date")
    lines = [f"- Домен: создан {created}."
             if not str(created).startswith("not_checked") else
             f"- Домен: дата создания не проверена ({created})."]
    if isinstance(facts.get("domain.age_years"), (int, float)):
        lines.append(f"- Возраст домена: {facts['domain.age_years']} лет.")
    if facts.get("domain.registrar"):
        lines.append(f"- Регистратор: {facts['domain.registrar']}.")
    sub = facts.get("domain.subdomains_count")
    if isinstance(sub, int):
        lines.append(f"- Субдоменов по crt.sh: {sub}.")
    return lines


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
        "msp": lambda: _fmt_msp(facts),
        "finances": lambda: ["- БФО (bo.nalog.gov.ru): не подключено — официальный "
                             "доступ ожидается (PRD §3, решение Ram 17.10)."],
        "bankruptcy": lambda: _fmt_bankruptcy(facts),
        "zalog": lambda: _fmt_zalog(facts),
        "courts": lambda: [
            "- Картотека арбитражных дел (kad.arbitr.ru): не подключена в M1–M2 "
            "(антибот; best-effort планируется в M3).",
            "- ФССП: исключён (капча, API отключён с 2022).",
        ],
        "reputation": lambda: [
            f"- Статус ФНС недобросовестных поставщиков: "
            f"{facts.get('npchk.state', '—')}",
            f"- Санкционный скрининг: "
            f"{'совпадений нет' if not data.get('sanctions_matches') else data['sanctions_matches']}",
        ],
        "digital": lambda: _fmt_digital(facts),
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
    facts = {f["field"]: f["value"] for f in data.get("facts", [])}
    filled = sum(1 for key in ALWAYS_FILLED if sections[key])
    filled += sum(1 for key, fact_key in FILLED_FACT.items()
                  if fact_key in facts)
    coverage = f"{filled}/{len(SECTION_KEYS)}"
    return {"sections": sections, "coverage": coverage,
            "disclaimer": DISCLAIMER, "module_version": MODULE_VERSION,
            "scoring_model_version":
            data.get("scorecard", {}).get("scoring_model_version", "v1")}


def render_markdown(data: dict) -> str:
    dossier = build_dossier(data)
    titles = {
        "summary": "Сводка", "registration": "Регистрация",
        "ownership": "Владение и управление", "msp": "МСП",
        "finances": "Финансы (БФО)", "bankruptcy": "Банкротство",
        "zalog": "Залоги", "courts": "Суды и исполнительные производства",
        "reputation": "Репутация и реестры", "digital": "Цифровой след",
        "red_flags": "Красные флаги", "conclusion": "Вывод",
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
