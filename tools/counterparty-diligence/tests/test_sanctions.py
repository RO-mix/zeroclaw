from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from cache import Cache  # noqa: E402
from sources.sanctions import (  # noqa: E402
    normalize_name, transliterate, name_variants, match_name,
    token_similarity, parse_ofac_sdn, parse_eu_fsd, parse_uk_ods,
    SanctionsScreener,
)

FIXTURES = Path(__file__).parent / "fixtures"


def _config():
    return {"sanctions_lists": {"ofac_sdn": {"url": "unused"},
                                "eu_fsd": {"url": "unused"},
                                "uk_oftsi": {"url": "unused"}},
            "sanctions": {"foreign": {"enabled": True}},
            "http": {"trust_env": False}}


# --- normalization ---

def test_normalize_strips_legal_forms_and_quotes():
    assert normalize_name('ООО "Рога и Копыта"') == "рога и копыта"
    assert normalize_name("ОАО «Газпром»") == "газпром"
    assert normalize_name('ROGA AND KOPYTA LLC') == "roga and kopyta"


def test_transliterate_both_directions():
    assert transliterate("рога", "ru_en") == "roga"
    assert transliterate("roga", "en_ru") == "рога"


def test_name_variants_produces_both_scripts():
    variants = name_variants("ООО Рога")
    assert "рога" in variants
    assert "roga" in variants  # cyrillic company screened against latin lists


# --- matching ---

def test_match_exact_and_fuzzy():
    assert match_name(["roga kopyta"], "ROGA AND KOPYTA LLC") == "fuzzy" or \
        match_name(["roga and kopyta"], "ROGA AND KOPYTA LLC") == "fuzzy"
    assert match_name(["ivan petrov"], "IVAN PETROV") == "exact"
    assert match_name(["totally different"], "IVAN PETROV") is None


def test_token_similarity_bounds():
    assert token_similarity("a b", "a b") == 1.0
    assert token_similarity("a b", "c d") == 0.0
    assert 0.8 <= token_similarity("ivan petrov", "ivan p petrov") <= 1.0


# --- list parsing ---

def test_parse_ofac_sdn_fixture():
    version, entries = parse_ofac_sdn(
        (FIXTURES / "ofac_sdn.xml").read_text("utf-8"))
    assert version == "10/16/2026"
    aliases = [e["alias"] for e in entries]
    assert "IVAN IVANOV" in aliases
    assert "ROGA AND KOPYTA LLC" in aliases


def test_parse_eu_fsd_fixture():
    version, entries = parse_eu_fsd(
        (FIXTURES / "eu_fsd.xml").read_text("utf-8"))
    assert version.startswith("2026-10-15")
    aliases = [e["alias"] for e in entries]
    assert "IVAN PETROV" in aliases
    assert "ZAO ZAKAT" in aliases  # raw alias; legal form stripped at normalization


def test_parse_uk_ods_fixture():
    data = (FIXTURES / "uk_sanctions_list.ods").read_bytes()
    version, entries = parse_uk_ods(data)
    assert version == "2026-10-16"  # "[Report for: 16-Oct-2026]"
    aliases = [e["alias"] for e in entries]
    assert "SERGEY SMIRNOV" in aliases        # Name 1 + Name 2 joined
    assert "S SMIRNOV" in aliases             # Alias row
    assert "СЕРГЕЙ СМИРНОВ" in aliases        # non-latin script variant
    assert aliases.count("REPEATED ENTITY") == 1  # rows-repeated expanded + deduped
    by_alias = {e["alias"]: e for e in entries}
    assert by_alias["S SMIRNOV"]["raw"]["uk_alias_strength"] == "strong"


def test_parse_uk_ods_last_modified_fallback():
    import zipfile
    import io as _io
    minimal = (b'<?xml version="1.0"?><office:document-content '
               b'xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" '
               b'xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" '
               b'xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0">'
               b'<office:body><office:spreadsheet><table:table>'
               b'<table:table-row><table:table-cell office:value-type="string">'
               b'<text:p>x</text:p></table:table-cell></table:table-row>'
               b'</table:table></office:spreadsheet></office:body>'
               b'</office:document-content>')
    buf = _io.BytesIO()
    with zipfile.ZipFile(buf, "w") as zf:
        zf.writestr("content.xml", minimal)
    version, entries = parse_uk_ods(
        buf.getvalue(), last_modified="Fri, 16 Oct 2026 09:30:38 GMT")
    assert version == "2026-10-16"  # HTTP-date parsed, not sliced
    assert entries == []


def test_screen_uk_entries_cross_script(tmp_path):
    cache = Cache(tmp_path / "test.sqlite")
    version, entries = parse_uk_ods(
        (FIXTURES / "uk_sanctions_list.ods").read_bytes())
    cache.replace_sanctions_list("uk_oftsi", [
        {"alias": e["alias"], "normalized_name": normalize_name(e["alias"]),
         "raw": e["raw"]} for e in entries], version)
    screener = SanctionsScreener(_config(), cache)
    matches = screener.screen("Smirnov Sergey")
    assert any(m["match"] == "fuzzy" for m in matches)
    matches = screener.screen("Сергей Смирнов")
    assert any(m["match"] == "exact" for m in matches)  # non-latin alias hit
    cache.close()


# --- screening against a real (tmp) cache ---

def _seeded_screener(tmp_path):
    cache = Cache(tmp_path / "test.sqlite")
    version, entries = parse_ofac_sdn(
        (FIXTURES / "ofac_sdn.xml").read_text("utf-8"))
    cache.replace_sanctions_list("ofac_sdn", [
        {"alias": e["alias"], "normalized_name": normalize_name(e["alias"]),
         "raw": e["raw"]} for e in entries], version)
    return SanctionsScreener(_config(), cache), cache


def test_screen_exact_and_cross_script(tmp_path):
    screener, cache = _seeded_screener(tmp_path)
    matches = screener.screen("Ivanov Ivan")
    assert any(m["match"] == "fuzzy" for m in matches)
    matches = screener.screen("ROGA AND KOPYTA OOO")
    assert any(m["list_name"] == "ofac_sdn" for m in matches)
    assert screener.screen("Чистое Небо ООО") == []
    cache.close()


def test_status_reports_versions(tmp_path):
    screener, cache = _seeded_screener(tmp_path)
    status = screener.status()
    assert status["ofac_sdn"]["version_date"] == "10/16/2026"
    cache.close()
