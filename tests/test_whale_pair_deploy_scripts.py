"""Tests for the Dublin whale-pair deployment scripts.

These tests exercise the surface of the helper shell scripts in
`scripts/deploy/whale-pair/` without requiring a Dublin host or Docker.
They guard against accidental regressions in the env contract,
script ergonomics, and compose-file shape.
"""

from __future__ import annotations

import os
import stat
import subprocess
from pathlib import Path

import pytest
import yaml

REPO_ROOT = Path(__file__).resolve().parent.parent
DEPLOY_DIR = REPO_ROOT / "scripts" / "deploy" / "whale-pair"
DOC_PATH = REPO_ROOT / "docs" / "deploy" / "whale-pair-dublin.md"

EXPECTED_SCRIPTS = [
    "check_env.sh",
    "provision.sh",
    "start.sh",
    "health.sh",
    "kill.sh",
]

VALID_FUNDER = "0xa57189d5b2285A5E64083d3925687bDFCE01fC83"
VALID_PRIVATE_KEY = "0x" + "a" * 64


def _write_env(tmp_path: Path, **overrides: str) -> Path:
    base = {
        "POLYMARKET_PRIVATE_KEY": VALID_PRIVATE_KEY,
        "POLYMARKET_SIGNATURE_TYPE": "1",
        "POLYMARKET_FUNDER": VALID_FUNDER,
        "STRATEGY_CONFIG": "/app/strategy_config.json",
    }
    base.update(overrides)
    env_file = tmp_path / ".env"
    env_file.write_text("\n".join(f"{k}={v}" for k, v in base.items() if v is not None) + "\n")
    env_file.chmod(0o600)
    return env_file


def _run_check_env(env_file: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["bash", str(DEPLOY_DIR / "check_env.sh"), str(env_file)],
        capture_output=True,
        text=True,
    )


class TestDeployDirectoryLayout:
    def test_deploy_directory_exists(self) -> None:
        assert DEPLOY_DIR.is_dir(), f"missing deploy dir: {DEPLOY_DIR}"

    def test_doc_exists_and_mentions_dublin(self) -> None:
        assert DOC_PATH.is_file(), f"missing spec doc: {DOC_PATH}"
        text = DOC_PATH.read_text()
        assert "Dublin" in text
        assert "Hetzner" in text, "spec must explain separation from Hetzner"
        assert "kill" in text.lower()
        assert "rollback" in text.lower()
        assert "dry-run" in text.lower()

    @pytest.mark.parametrize("name", EXPECTED_SCRIPTS)
    def test_script_present_and_executable(self, name: str) -> None:
        path = DEPLOY_DIR / name
        assert path.is_file(), f"missing script: {path}"
        mode = path.stat().st_mode
        assert mode & stat.S_IXUSR, f"{path} is not executable"

    def test_compose_file_present(self) -> None:
        compose = DEPLOY_DIR / "docker-compose.whale-pair.yml"
        assert compose.is_file()


class TestComposeFile:
    """Verify the Dublin compose file is shaped correctly and stays separate
    from the repo-root Hetzner compose file."""

    def _load(self) -> dict:
        compose = DEPLOY_DIR / "docker-compose.whale-pair.yml"
        with compose.open() as fh:
            return yaml.safe_load(fh)

    def test_contains_whale_pair_live_service(self) -> None:
        data = self._load()
        assert "services" in data
        assert "whale-pair-live" in data["services"]

    def test_runs_whale_pair_live_bot_script(self) -> None:
        svc = self._load()["services"]["whale-pair-live"]
        entrypoint = svc.get("entrypoint", [])
        assert "scripts/whale_pair_live_bot.py" in entrypoint

    def test_uses_repo_env_file(self) -> None:
        svc = self._load()["services"]["whale-pair-live"]
        assert svc.get("env_file") == ["../../../.env"]

    def test_conservative_defaults_for_first_shift(self) -> None:
        """First-shift caps must be reduced from the script defaults."""
        svc = self._load()["services"]["whale-pair-live"]
        command = svc.get("command", [])
        # base-clip defaults to 50.0 in the script; compose default must be
        # smaller for the first live shift.
        assert "--base-clip-usd" in command
        base_clip_idx = command.index("--base-clip-usd")
        base_clip_value = command[base_clip_idx + 1]
        # Compose uses env interpolation with default fallback.
        assert "10.0" in base_clip_value or base_clip_value.startswith("${"), (
            f"first-shift --base-clip-usd should default to a reduced value, got {base_clip_value!r}"
        )
        assert "--max-gross-cost-usd" in command

    def test_does_not_include_execute_flag_by_default(self) -> None:
        """Base compose file must be dry-run safe; --execute is added by start.sh --live only."""
        svc = self._load()["services"]["whale-pair-live"]
        command = svc.get("command", [])
        assert "--execute" not in command, (
            "base compose must not carry --execute; live mode adds it via override"
        )

    def test_root_compose_not_modified(self) -> None:
        """The Hetzner stack compose file must not mention whale-pair-live."""
        root_compose = REPO_ROOT / "docker-compose.yml"
        with root_compose.open() as fh:
            root = yaml.safe_load(fh)
        services = root.get("services", {})
        assert "whale-pair-live" not in services, (
            "whale-pair-live must stay out of the Hetzner root docker-compose.yml"
        )


class TestCheckEnv:
    def test_accepts_well_formed_env(self, tmp_path: Path) -> None:
        env_file = _write_env(tmp_path)
        result = _run_check_env(env_file)
        assert result.returncode == 0, result.stderr

    def test_rejects_missing_private_key(self, tmp_path: Path) -> None:
        env_file = _write_env(tmp_path, POLYMARKET_PRIVATE_KEY=None)  # type: ignore[arg-type]
        result = _run_check_env(env_file)
        assert result.returncode != 0
        assert "POLYMARKET_PRIVATE_KEY" in result.stderr

    def test_rejects_missing_funder(self, tmp_path: Path) -> None:
        env_file = _write_env(tmp_path, POLYMARKET_FUNDER=None)  # type: ignore[arg-type]
        result = _run_check_env(env_file)
        assert result.returncode != 0
        assert "POLYMARKET_FUNDER" in result.stderr

    def test_rejects_missing_strategy_config(self, tmp_path: Path) -> None:
        env_file = _write_env(tmp_path, STRATEGY_CONFIG=None)  # type: ignore[arg-type]
        result = _run_check_env(env_file)
        assert result.returncode != 0
        assert "STRATEGY_CONFIG" in result.stderr

    def test_rejects_bad_signature_type(self, tmp_path: Path) -> None:
        env_file = _write_env(tmp_path, POLYMARKET_SIGNATURE_TYPE="9")
        result = _run_check_env(env_file)
        assert result.returncode != 0
        assert "POLYMARKET_SIGNATURE_TYPE" in result.stderr

    def test_rejects_non_hex_funder(self, tmp_path: Path) -> None:
        env_file = _write_env(tmp_path, POLYMARKET_FUNDER="0xnothex")
        result = _run_check_env(env_file)
        assert result.returncode != 0
        assert "POLYMARKET_FUNDER" in result.stderr

    def test_rejects_short_private_key(self, tmp_path: Path) -> None:
        env_file = _write_env(tmp_path, POLYMARKET_PRIVATE_KEY="0xabc")
        result = _run_check_env(env_file)
        assert result.returncode != 0
        assert "POLYMARKET_PRIVATE_KEY" in result.stderr

    def test_rejects_world_readable_env(self, tmp_path: Path) -> None:
        env_file = _write_env(tmp_path)
        env_file.chmod(0o644)
        result = _run_check_env(env_file)
        assert result.returncode != 0
        assert "permissions" in result.stderr.lower()

    def test_rejects_missing_file(self, tmp_path: Path) -> None:
        result = subprocess.run(
            ["bash", str(DEPLOY_DIR / "check_env.sh"), str(tmp_path / "nope.env")],
            capture_output=True,
            text=True,
        )
        assert result.returncode != 0
        assert "not found" in result.stderr.lower()


class TestScriptUsage:
    """Scripts should print a usage message and exit non-zero with no args."""

    @pytest.mark.parametrize("name", ["start.sh", "kill.sh", "provision.sh"])
    def test_prints_usage_without_args(self, name: str) -> None:
        result = subprocess.run(
            ["bash", str(DEPLOY_DIR / name)],
            capture_output=True,
            text=True,
        )
        assert result.returncode != 0
        assert "Usage" in result.stdout + result.stderr

    def test_start_rejects_unknown_mode(self) -> None:
        result = subprocess.run(
            ["bash", str(DEPLOY_DIR / "start.sh"), "--banana"],
            capture_output=True,
            text=True,
        )
        assert result.returncode != 0

    def test_kill_rejects_unknown_mode(self) -> None:
        result = subprocess.run(
            ["bash", str(DEPLOY_DIR / "kill.sh"), "--banana"],
            capture_output=True,
            text=True,
        )
        assert result.returncode != 0


class TestDocContract:
    """The spec doc is part of the deployment artifact. These assertions keep
    it honest about the things a runbook reader relies on."""

    def test_doc_mentions_required_env_vars(self) -> None:
        text = DOC_PATH.read_text()
        for var in (
            "POLYMARKET_PRIVATE_KEY",
            "POLYMARKET_SIGNATURE_TYPE",
            "POLYMARKET_FUNDER",
            "STRATEGY_CONFIG",
        ):
            assert var in text, f"spec doc must mention {var}"

    def test_doc_mentions_whale_pair_live_bot(self) -> None:
        text = DOC_PATH.read_text()
        assert "whale_pair_live_bot.py" in text

    def test_doc_explains_why_not_hetzner(self) -> None:
        text = DOC_PATH.read_text().lower()
        assert "hetzner" in text
        assert "falkenstein" in text
        # We explicitly call out that Hetzner has no Dublin region.
        assert "dublin" in text

    def test_doc_lists_inputs_still_needed(self) -> None:
        text = DOC_PATH.read_text().lower()
        # The spec must call out the still-unknown operator inputs.
        assert "still" in text or "unknown" in text
        assert "private key" in text or "wallet" in text
