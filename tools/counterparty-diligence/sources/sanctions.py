"""Sanctions lists (OFAC SDN, EU FSD, UK OFSI): load, normalize, screen.

Loading: fetch the official XML per list, parse into alias records and store
them in the sqlite cache (table sanctions_entries) with a version date.
Screening: normalize a company name and match it against cached aliases by
token containment or >= 0.8 token-set similarity.
"""

from __future__ import annotations

import re
from datetime import datetime, timezone
from typing import Any, Optional
from xml.etree import ElementTree as ET

from .base import http_timeout, make_session

# --- name normalization -------------------------------------------------

RU_LEGAL_FORMS = ("общество с ограниченной ответственностью", "акционерное общество",
                  "публичное акционерное общество", "закрытое акционерное общество",
                  "открытое акционерное общество", "индивидуальный предприниматель")
RU_LEGAL_ABBR = ("ооо", "оао", "зао", "пао", "ао", "ип", "нп", "гуп", "муп", "фгуп")
EN_LEGAL_FORMS = ("ltd", "llc", "inc", "corp", "co", "jsc", "llp", "plc", "gmbh",
                  "s.a", "sa", "bv", "oy", "ab", "as", "oyj", "pty")
QUOTE_CHARS = "\"'«»„“”‘’`´"

# Simplified RU -> EN transliteration table (PRD: упрощённая таблица).
TRANSLIT_RU_EN = {
    "а": "a", "б": "b", "в": "v", "г": "g", "д": "d", "е": "e", "ё": "e",
    "ж": "zh", "з": "z", "и": "i", "й": "y", "к": "k", "л": "l", "м": "m",
    "н": "n", "о": "o", "п": "p", "р": "r", "с": "s", "т": "t", "у": "u",
    "ф": "f", "х": "h", "ц": "c", "ч": "ch", "ш": "sh", "щ": "sch",
    "ъ": "", "ы": "y", "ь": "", "э": "e", "ю": "yu", "я": "ya",
}
EN_TO_RU = {v: k for k, v in TRANSLIT_RU_EN.items() if v}


def transliterate(text: str, direction: str = "ru_en") -> str:
    table = TRANSLIT_RU_EN if direction == "ru_en" else EN_TO_RU
    if direction == "en_ru":
        # greedy two-letter matches (zh, ch, sh, sch, yu, ya) first
        for pair in ("sch", "zh", "ch", "sh", "yu", "ya"):
            text = text.replace(pair, table.get(pair, pair))
    return "".join(table.get(ch, ch) for ch in text)


def normalize_name(name: str) -> str:
    """Lowercase, strip quotes/legal forms, collapse to a token string."""
    text = name.lower().strip()
    for ch in QUOTE_CHARS:
        text = text.replace(ch, " ")
    for form in RU_LEGAL_FORMS:
        text = text.replace(form, " ")
    text = re.sub(r"[^0-9a-zа-яё]+", " ", text)
    tokens = [t for t in text.split()
              if t not in RU_LEGAL_ABBR and t not in EN_LEGAL_FORMS]
    return " ".join(tokens)


def name_variants(name: str) -> list[str]:
    """Normalized name in both scripts (latin <-> cyrillic)."""
    norm = normalize_name(name)
    variants = {norm}
    if any("а" <= ch <= "я" or ch == "ё" for ch in norm):
        variants.add(normalize_name(transliterate(norm, "ru_en")))
    elif any("a" <= ch <= "z" for ch in norm):
        variants.add(normalize_name(transliterate(norm, "en_ru")))
    return [v for v in variants if v]


# --- list parsing -------------------------------------------------------

def _strip_ns(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def _text(el: ET.Element) -> str:
    return (el.text or "").strip()


def parse_ofac_sdn(xml_text: str) -> tuple[str, list[dict]]:
    """OFAC SDN XML -> (version_date, [{alias, raw}])."""
    root = ET.fromstring(xml_text)
    version = datetime.now(timezone.utc).date().isoformat()
    for el in root.iter():
        if _strip_ns(el.tag) == "Publish_Date":
            version = _text(el) or version
            break
    entries = []
    for el in root.iter():
        if _strip_ns(el.tag) != "sdnEntry":
            continue
        first = last = ""
        for child in el:
            if _strip_ns(child.tag) == "firstName":
                first = _text(child)
            elif _strip_ns(child.tag) == "lastName":
                last = _text(child)
        names = [" ".join(p for p in (first, last) if p)] if last else []
        for child in el.iter():
            if _strip_ns(child.tag) == "aka":
                parts = [_text(g) for g in child if _strip_ns(g.tag) in
                         ("firstName", "lastName", "category")]
                name = " ".join(p for p in parts[:2] if p)
                if name:
                    names.append(name)
        for name in names:
            entries.append({"alias": name, "raw": {"sdn_name": name}})
    return version, entries


def parse_eu_fsd(xml_text: str) -> tuple[str, list[dict]]:
    """EU FSF full sanctions list XML -> (version_date, [{alias, raw}])."""
    root = ET.fromstring(xml_text)
    version = datetime.now(timezone.utc).date().isoformat()
    for attr in ("date", "generationDate", "exportDate", "Version"):
        if root.get(attr):
            version = root.get(attr)[:10]
            break
    for el in root.iter():
        if _strip_ns(el.tag) in ("exportDate", "generationDate"):
            version = (_text(el) or version)[:10]
            break
    entries = []
    for el in root.iter():
        if _strip_ns(el.tag) != "nameAlias":
            continue
        name = (el.get("wholeName") or "").strip()
        if not name:
            parts = [el.get("firstName") or "", el.get("lastName") or "",
                     el.get("middleName") or ""]
            name = " ".join(p for p in parts if p).strip()
        if name:
            entries.append({"alias": name,
                            "raw": {"eu_wholeName": el.get("wholeName") or name,
                                    "phonetic": el.get("phonetic") or ""}})
    return version, entries


def parse_uk_list(xml_text: str) -> tuple[str, list[dict]]:
    """UK Sanctions List XML -> (version_date, [{alias, raw}])."""
    root = ET.fromstring(xml_text)
    version = datetime.now(timezone.utc).date().isoformat()
    for attr in ("Version", "GeneratedAt", "LastUpdated"):
        if root.get(attr):
            version = root.get(attr)[:10]
            break
    for el in root.iter():
        if _strip_ns(el.tag) in ("GeneratedAt", "Version", "LastUpdated"):
            version = (_text(el) or version)[:10]
            break
    entries = []
    for el in root.iter():
        if _strip_ns(el.tag) not in ("Name", "Alias"):
            continue
        name = (el.get("Name") or "").strip()
        if name:
            entries.append({"alias": name, "raw": {"uk_name": name}})
    return version, entries


LIST_PARSERS = {
    "ofac_sdn": parse_ofac_sdn,
    "eu_fsd": parse_eu_fsd,
    "uk_oftsi": parse_uk_list,
}


# --- screening ----------------------------------------------------------

def token_similarity(a: str, b: str) -> float:
    """Dice coefficient over token multisets, in [0, 1]."""
    ta, tb = a.split(), b.split()
    if not ta or not tb:
        return 0.0
    from collections import Counter
    inter = sum((Counter(ta) & Counter(tb)).values())
    return 2 * inter / (len(ta) + len(tb))


def match_name(query_variants: list[str], alias: str,
               fuzzy_threshold: float = 0.8) -> Optional[str]:
    """Return "exact" | "fuzzy" | None for one normalized alias."""
    norm_alias = normalize_name(alias)
    if not norm_alias:
        return None
    alias_tokens = set(norm_alias.split())
    for variant in query_variants:
        if variant == norm_alias:
            return "exact"
        query_tokens = set(variant.split())
        if query_tokens and query_tokens <= alias_tokens:
            return "fuzzy"
        if token_similarity(variant, norm_alias) >= fuzzy_threshold:
            return "fuzzy"
    return None


class SanctionsScreener:
    """Downloads lists into the cache and screens names against it."""

    def __init__(self, config: dict, cache, session: Optional[Any] = None):
        self.config = config
        self.cache = cache
        self._session = session or make_session(config)
        self._lists = config.get("sanctions_lists", {})

    # loading

    def refresh_list(self, list_name: str) -> dict:
        url = self._lists.get(list_name, {}).get("url")
        if not url:
            raise ValueError(f"no URL configured for sanctions list {list_name}")
        resp = self._session.get(url, timeout=http_timeout(self.config))
        resp.raise_for_status()
        version, entries = LIST_PARSERS[list_name](resp.content.decode("utf-8", "replace"))
        normalized = [
            {"alias": e["alias"],
             "normalized_name": normalize_name(e["alias"]),
             "raw": e["raw"]}
            for e in entries if normalize_name(e["alias"])
        ]
        self.cache.replace_sanctions_list(list_name, normalized, version)
        return {"list_name": list_name, "version_date": version,
                "entries": len(normalized)}

    def refresh_all(self) -> list[dict]:
        results = []
        for list_name in self._lists:
            try:
                results.append(self.refresh_list(list_name))
            except Exception as exc:  # degrade gracefully (PRD FR9)
                results.append({"list_name": list_name, "error": str(exc)})
        return results

    # screening

    def screen(self, name: str, fuzzy_threshold: float = 0.8) -> list[dict]:
        variants = name_variants(name)
        matches = []
        for entry in self.cache.all_sanctions_entries():
            match_type = match_name(variants, entry["alias"], fuzzy_threshold)
            if match_type:
                matches.append({"list_name": entry["list_name"],
                                "alias": entry["alias"],
                                "match": match_type,
                                "version_date": entry["version_date"]})
        return matches

    def status(self) -> dict:
        out = {}
        for list_name in self._lists:
            out[list_name] = self.cache.get_list_version(list_name)
        return out
