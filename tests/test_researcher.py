"""Tests for the autonomous researcher."""
from __future__ import annotations

import tempfile
from pathlib import Path
from unittest.mock import patch

import pytest

from researcher.researcher import write_insight, generate_researcher_prompt, INSIGHTS_DIR


def test_write_insight(tmp_path: Path):
    with patch("researcher.researcher.INSIGHTS_DIR", tmp_path):
        path = write_insight(
            title="Test Finding",
            source="Twitter",
            priority="high",
            finding="Found a new strategy",
            trading_implication="Could improve win rate",
            suggested_action="Test in autoresearch",
            raw_data="https://twitter.com/example",
        )
        assert path.exists()
        content = path.read_text()
        assert "Test Finding" in content
        assert "Twitter" in content
        assert "high" in content
        assert "Found a new strategy" in content
        assert "https://twitter.com/example" in content


def test_write_insight_without_raw_data(tmp_path: Path):
    with patch("researcher.researcher.INSIGHTS_DIR", tmp_path):
        path = write_insight(
            title="Minimal Finding",
            source="GitHub",
            priority="low",
            finding="Nothing major",
            trading_implication="None",
            suggested_action="None",
        )
        content = path.read_text()
        assert "Raw Data" not in content


def test_generate_researcher_prompt():
    prompt = generate_researcher_prompt()
    assert "polymarket" in prompt.lower()
    assert "Twitter" in prompt
    assert "GitHub" in prompt
    assert "Leaderboard" in prompt
    assert len(prompt) > 200
