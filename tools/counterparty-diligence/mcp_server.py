"""stdio MCP server exposing counterparty-diligence tools.

Tools:
  check(inn?, name?, level="quick") -> structured result + scoring
  dossier(inn, format="md")         -> markdown/JSON report
  sources_status()                  -> availability snapshot
"""

from __future__ import annotations

import asyncio
from typing import Optional

from mcp.server.mcpserver import MCPServer

from service import DiligenceService  # absolute: dir name is not a package id

mcp = MCPServer("counterparty-diligence")
_service: Optional[DiligenceService] = None


def _get_service() -> DiligenceService:
    global _service
    if _service is None:
        _service = DiligenceService()
    return _service


@mcp.tool()
async def check(inn: Optional[str] = None, name: Optional[str] = None,
                level: str = "quick") -> dict:
    """Check a Russian counterparty by INN (10/12 digits) or exact name.

    Gathers facts from FNS registries (EGRUL, stop-lists, dishonest
    suppliers) and sanctions screening, then scores them 0-100.
    level: quick (M1) | standard | full (M2+ sources degrade to warnings).
    """
    result = await asyncio.to_thread(_get_service().check,
                                     inn=inn, name=name, level=level)
    return result


@mcp.tool()
async def dossier(inn: str, format: str = "md") -> str:
    """Build a full diligence dossier for an INN.

    format: "md" (default, structured markdown) or "json" (raw dict).
    Includes coverage, scoring model version and a not-a-legal-opinion
    disclaimer. Requires unambiguous identification by INN.
    """
    if format not in ("md", "json"):
        raise ValueError("format must be md|json")
    return await asyncio.to_thread(_get_service().dossier, inn=inn, fmt=format)


@mcp.tool()
async def sources_status() -> dict:
    """Report data-source availability: sanctions list versions from cache,
    live registries probed lazily on first use."""
    return await asyncio.to_thread(_get_service().sources_status)


def main() -> None:
    mcp.run(transport="stdio")


if __name__ == "__main__":
    main()
