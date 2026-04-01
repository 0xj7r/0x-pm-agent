"""CLI entry point for the autonomous researcher.

Usage:
    # Run with Claude Code (recommended — has web search)
    claude --prompt "$(cat researcher/researcher.py | python -c 'from researcher.researcher import generate_researcher_prompt; print(generate_researcher_prompt())')"

    # Run as a standalone script that outputs the prompt
    python researcher/run_research.py --prompt  # prints the researcher prompt
    python researcher/run_research.py --scan    # runs scan with Claude API

The researcher is designed to be invoked by:
1. A cron job on Hetzner (daily at 6 AM UTC)
2. Claude Code with web search capabilities
3. Manual invocation when you want fresh intelligence
"""
from __future__ import annotations

import argparse
import json
import logging
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from researcher.researcher import (
    INSIGHTS_DIR,
    generate_researcher_prompt,
    write_insight,
)

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
)
logger = logging.getLogger(__name__)


def run_with_claude_api() -> None:
    """Run the researcher using the Claude API directly."""
    api_key = os.getenv("ANTHROPIC_API_KEY")
    if not api_key:
        logger.error("ANTHROPIC_API_KEY not set. Cannot run autonomous research.")
        logger.info("Alternative: use Claude Code with web search capabilities.")
        logger.info("Run: claude --prompt researcher/researcher_prompt.md")
        sys.exit(1)

    try:
        import anthropic
    except ImportError:
        logger.error("anthropic package not installed. Run: pip install anthropic")
        sys.exit(1)

    client = anthropic.Anthropic(api_key=api_key)
    prompt = generate_researcher_prompt()

    logger.info("Running autonomous research with Claude API...")
    response = client.messages.create(
        model=os.getenv("CLAUDE_MODEL", "claude-sonnet-4-20250514"),
        max_tokens=4096,
        messages=[{"role": "user", "content": prompt}],
    )

    content = response.content[0].text
    logger.info(f"Research response ({len(content)} chars):")
    print(content)

    # Save the raw response as an insight
    write_insight(
        title="Automated Research Scan",
        source="Claude API",
        priority="medium",
        finding=content[:500],
        trading_implication="See full response below",
        suggested_action="Review findings and feed into autoresearch loop",
        raw_data=content,
    )


def list_insights() -> None:
    """List all research insights."""
    if not INSIGHTS_DIR.exists():
        print("No insights directory found.")
        return

    files = sorted(INSIGHTS_DIR.glob("*.md"))
    if not files:
        print("No insights found.")
        return

    for f in files:
        first_line = f.read_text().split("\n")[0]
        print(f"  {f.name}: {first_line}")


def main() -> None:
    parser = argparse.ArgumentParser(description="Polymarket Autonomous Researcher")
    parser.add_argument("--prompt", action="store_true", help="Print the researcher prompt")
    parser.add_argument("--scan", action="store_true", help="Run research scan via Claude API")
    parser.add_argument("--list", action="store_true", help="List existing insights")
    args = parser.parse_args()

    if args.prompt:
        print(generate_researcher_prompt())
    elif args.scan:
        run_with_claude_api()
    elif args.list:
        list_insights()
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
