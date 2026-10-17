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
    """EU FSF full sanctions list XML -> (version_date, [{alias, raw}]).

    Note: webgate geoblocks bare /content URLs (403 from RU networks); use
    the token-suffixed distribution URL from the data.europa.eu catalog.
    """
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


# --- UK Sanctions List (O DS from sanctionslist.fcdo.gov.uk) -------------

_ODF_NS = {
    "table": "urn:oasis:names:tc:opendocument:xmlns:table:1.0",
    "text": "urn:oasis:names:tc:opendocument:xmlns:text:1.0",
}
_ODF_TABLE, _ODF_TEXT_P = (_ODF_NS["table"], _ODF_NS["text"])
_MAX_ROW_REPEAT = 200_000  # guard against runaway number-rows-repeated
_MONTHS = {"Jan": "01", "Feb": "02", "Mar": "03", "Apr": "04", "May": "05",
           "Jun": "06", "Jul": "07", "Aug": "08", "Sep": "09", "Oct": "10",
           "Nov": "11", "Dec": "12"}


def _ods_cell_text(cell: ET.Element) -> str:
    return " ".join("".join(p.itertext())
                    for p in cell.iter(f"{{{_ODF_TEXT_P}}}p")).strip()


def _ods_rows(table: ET.Element) -> list[list[str]]:
    """Expand an ODS table into logical rows of cell strings."""
    rows = []
    for row in table.iter(f"{{{_ODF_TABLE}}}table-row"):
        repeat = int(row.get(f"{{{_ODF_TABLE}}}number-rows-repeated") or 1)
        cells = []
        for cell in row.iter(f"{{{_ODF_TABLE}}}table-cell"):
            col_repeat = int(
                cell.get(f"{{{_ODF_TABLE}}}number-columns-repeated") or 1)
            text = _ods_cell_text(cell)
            cells.extend([text] * min(col_repeat, 512))
        for _ in range(min(repeat, _MAX_ROW_REPEAT)):
            rows.append(list(cells))
    return rows


def parse_uk_ods(data: bytes,
                 last_modified: Optional[str] = None) -> tuple[str, list[dict]]:
    """UK Sanctions List ODS -> (version_date, [{alias, raw}]).

    Parses content.xml directly (stdlib zipfile+ElementTree): odfpy's DOM
    loader cannot handle the live ~8 MB file in reasonable time. Main sheet
    "UK Sanctions List": row 0 carries "[Report for: DD-Mon-YYYY]", row 1 is
    the header (Name 1..6 parts, Name type, Alias strength, Name non-latin
    script). Primary names and aliases both become screening entries;
    non-latin-script variants are added as extra aliases.
    """
    import io
    import zipfile

    version = datetime.now(timezone.utc).date().isoformat()
    if last_modified:
        try:
            from email.utils import parsedate_to_datetime
            version = parsedate_to_datetime(last_modified).date().isoformat()
        except (TypeError, ValueError, IndexError, OverflowError):
            version = last_modified[:10]
    with zipfile.ZipFile(io.BytesIO(data)) as zf:
        root = ET.fromstring(zf.read("content.xml"))
    table = root.find(f".//{{{_ODF_TABLE}}}table")
    if table is None:
        return version, []
    rows = _ods_rows(table)
    if not rows:
        return version, []

    report = " ".join(x for x in rows[0] if x)
    m = re.search(r"(\d{2})-(\w{3})-(\d{4})", report)
    if m and m.group(2) in _MONTHS:
        version = f"{m.group(3)}-{_MONTHS[m.group(2)]}-{m.group(1)}"

    header = rows[1] if len(rows) > 1 else []
    name_idx = [i for i, h in enumerate(header)
                if re.fullmatch(r"Name [1-6]", h or "")]
    type_idx = header.index("Name type") if "Name type" in header else None
    strength_idx = (header.index("Alias strength")
                    if "Alias strength" in header else None)
    nonlatin_idx = (header.index("Name non-latin script")
                    if "Name non-latin script" in header else None)

    entries = []
    seen = set()
    for cells in rows[2:]:
        parts = [cells[i] for i in name_idx if i < len(cells) and cells[i]]
        name = " ".join(parts).strip()
        if not name:
            continue
        raw = {
            "uk_name_type": cells[type_idx] if type_idx is not None and type_idx < len(cells) else "",
            "uk_alias_strength": (cells[strength_idx]
                                  if strength_idx is not None and strength_idx < len(cells) else ""),
        }
        for alias in [name] + (
                [cells[nonlatin_idx]] if nonlatin_idx is not None
                and nonlatin_idx < len(cells) and cells[nonlatin_idx] else []):
            if alias not in seen:
                seen.add(alias)
                entries.append({"alias": alias, "raw": raw})
    return version, entries


LIST_PARSERS = {
    "ofac_sdn": lambda data, _lm: parse_ofac_sdn(
        data.decode("utf-8", "replace")),
    "eu_fsd": lambda data, _lm: parse_eu_fsd(
        data.decode("utf-8", "replace")),
    "uk_oftsi": parse_uk_ods,
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
    """Downloads lists into the cache and screens names against it.

    RU-focus policy: foreign lists (OFAC/EU/UK) are optional; when
    sanctions.foreign.enabled is false the screener never touches them —
    screen() returns [], status() reports "disabled (ru-focus)".
    """

    def __init__(self, config: dict, cache, session: Optional[Any] = None):
        self.config = config
        self.cache = cache
        self._session = session or make_session(config)
        self._lists = config.get("sanctions_lists", {})
        self.foreign_enabled = bool(
            config.get("sanctions", {}).get("foreign", {}).get("enabled", False))

    # loading

    def refresh_list(self, list_name: str) -> dict:
        if not self.foreign_enabled:
            return {"list_name": list_name,
                    "error": "disabled (ru-focus)"}
        url = self._lists.get(list_name, {}).get("url")
        if not url:
            raise ValueError(f"no URL configured for sanctions list {list_name}")
        resp = self._session.get(url, timeout=http_timeout(self.config))
        try:
            resp.raise_for_status()
        except Exception as exc:
            if list_name == "eu_fsd" and getattr(resp, "status_code", 0) == 403:
                raise RuntimeError(
                    "EU FSD geoblocked (403) from this network; update the URL "
                    "token from the data.europa.eu catalog or refresh manually "
                    "via VPN/external host"
                ) from exc
            raise
        last_modified = resp.headers.get("Last-Modified", "")
        version, entries = LIST_PARSERS[list_name](resp.content, last_modified)
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
        if not self.foreign_enabled:
            return []
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
        if not self.foreign_enabled:
            return {"enabled": False, "note": "disabled (ru-focus)"}
        out = {}
        for list_name in self._lists:
            out[list_name] = self.cache.get_list_version(list_name)
        return out
