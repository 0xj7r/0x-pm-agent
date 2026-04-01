"""Autonomous researcher for Polymarket trading edge discovery.

Scans external sources for new strategies, profitable wallets,
market microstructure changes, and competitive intelligence.
Outputs structured findings to research_insights/ for the
autoresearch loop to consume.

Designed to run as a daily cron job or on-demand.
"""
from __future__ import annotations

import json
import logging
import os
from datetime import datetime, timezone
from pathlib import Path

logger = logging.getLogger(__name__)

INSIGHTS_DIR = Path("research_insights")


def _ensure_insights_dir() -> None:
    INSIGHTS_DIR.mkdir(exist_ok=True)


def write_insight(
    title: str,
    source: str,
    priority: str,
    finding: str,
    trading_implication: str,
    suggested_action: str,
    raw_data: str = "",
) -> Path:
    """Write a structured research insight to disk."""
    _ensure_insights_dir()

    timestamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    slug = title.lower().replace(" ", "_")[:40]
    filename = f"{timestamp}_{slug}.md"
    path = INSIGHTS_DIR / filename

    content = f"""# Research Insight: {title}
Date: {datetime.now(timezone.utc).isoformat()}
Source: {source}
Priority: {priority}

## Finding
{finding}

## Trading Implication
{trading_implication}

## Suggested Action
{suggested_action}
"""
    if raw_data:
        content += f"""
## Raw Data
{raw_data}
"""

    path.write_text(content)
    logger.info(f"Wrote insight: {path}")
    return path


def scan_polymarket_leaderboard() -> list[dict]:
    """Scan Polymarket leaderboard for profitable BTC traders.

    Returns list of findings about top wallets and their patterns.
    Requires ANTHROPIC_API_KEY for Claude-powered analysis.
    """
    # This will use Claude to analyze leaderboard data
    # For now, returns structure for the researcher agent to fill
    return []


def scan_github_repos() -> list[dict]:
    """Search GitHub for new Polymarket bot repos.

    Tracks: new repos, star spikes on known repos, strategy changes.
    """
    return []


def scan_twitter() -> list[dict]:
    """Search Twitter/X for Polymarket strategy discussions.

    Keywords: polymarket bot, up or down, btc binary, prediction market arb
    """
    return []


def scan_fee_changes() -> list[dict]:
    """Check Polymarket docs for fee structure or rule changes."""
    return []


def generate_researcher_prompt() -> str:
    """Generate the prompt for Claude to act as the autonomous researcher.

    This prompt is used with the Claude API or Claude Code to perform
    the actual research. The researcher.py module provides the structure;
    Claude provides the intelligence.
    """
    return """You are an autonomous researcher for a Polymarket BTC trading bot.

Your job is to find new trading edges by scanning public sources.

## What to Search For

1. **Twitter/X** — Search for:
   - "polymarket bot" strategy discussions
   - "polymarket up or down" analysis
   - Wallet analysis threads (like @Dan1ro0's posts)
   - "polymarket arbitrage" new techniques
   - Any discussion of Polymarket fee changes

2. **GitHub** — Search for:
   - New repos: "polymarket bot", "polymarket trading", "prediction market maker"
   - Activity on known repos: Polymarket/agents, warproxxx/poly-maker
   - New Avellaneda-Stoikov or Bayesian implementations for binary markets

3. **Polymarket Leaderboard** — Check:
   - Top wallets on BTC Up/Down markets this week
   - New wallets appearing with high win rates
   - Trading patterns: timing, sizing, frequency

4. **Market Microstructure** — Check:
   - Current spread on BTC Up/Down markets
   - Liquidity depth at different price levels
   - Fee structure changes (check docs.polymarket.com)

## Output Format

For each finding, call the write_insight function with:
- title: Clear, descriptive title
- source: "Twitter" | "GitHub" | "Leaderboard" | "Microstructure" | "Docs"
- priority: "high" | "medium" | "low"
- finding: What you discovered
- trading_implication: How this affects our strategy
- suggested_action: What the autoresearch loop should try

## Rules

- Only report genuinely actionable findings
- "high" priority = immediate strategy change needed
- "medium" priority = worth testing in autoresearch
- "low" priority = interesting but not urgent
- Include links/evidence in raw_data
- If nothing new found, that's fine — say so
"""
