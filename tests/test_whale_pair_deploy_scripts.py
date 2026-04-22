"""Tests for the Dublin whale-pair deployment scripts.

These tests exercise the surface of the helper shell scripts in
`scripts/deploy/whale-pair/` without requiring a Dublin host or Docker.
They guard against accidental regressions in the env contract,
script ergonomics, and compose-file shape.
"""

from __future__ import annotations

import json
import os
import sqlite3
import stat
import subprocess
from pathlib import Path

import pytest
import yaml

REPO_ROOT = Path(__file__).resolve().parent.parent
DEPLOY_DIR = REPO_ROOT / "scripts" / "deploy" / "whale-pair"
DOC_PATH = REPO_ROOT / "docs" / "deploy" / "whale-pair-dublin.md"
BACKUP_DOC_PATH = REPO_ROOT / "docs" / "deploy" / "whale-pair-backup-recovery.md"
STANDBY_DOC_PATH = REPO_ROOT / "docs" / "deploy" / "whale-pair-standby.md"
MONITOR_DOC_PATH = REPO_ROOT / "docs" / "deploy" / "whale-pair-monitoring.md"
OPS_DIR = REPO_ROOT / "ops"

EXPECTED_SCRIPTS = [
    "check_env.sh",
    "provision.sh",
    "start.sh",
    "health.sh",
    "kill.sh",
    "backup_data.sh",
    "restore_data.sh",
    "standby_bootstrap.sh",
    "standby_status.sh",
]

EXPECTED_MONITORING_SCRIPTS = [
    "start_monitoring.sh",
    "stop_monitoring.sh",
    "refresh_monitoring_metrics.sh",
    "install_monitoring_cron.sh",
    "monitoring_health.sh",
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

    def test_backup_and_standby_docs_exist(self) -> None:
        assert BACKUP_DOC_PATH.is_file(), f"missing backup doc: {BACKUP_DOC_PATH}"
        assert STANDBY_DOC_PATH.is_file(), f"missing standby doc: {STANDBY_DOC_PATH}"

    @pytest.mark.parametrize("name", EXPECTED_SCRIPTS)
    def test_script_present_and_executable(self, name: str) -> None:
        path = DEPLOY_DIR / name
        assert path.is_file(), f"missing script: {path}"
        mode = path.stat().st_mode
        assert mode & stat.S_IXUSR, f"{path} is not executable"

    @pytest.mark.parametrize("name", EXPECTED_MONITORING_SCRIPTS)
    def test_monitoring_script_present_and_executable(self, name: str) -> None:
        path = DEPLOY_DIR / name
        assert path.is_file(), f"missing monitoring script: {path}"
        mode = path.stat().st_mode
        assert mode & stat.S_IXUSR, f"{path} is not executable"

    def test_compose_file_present(self) -> None:
        compose = DEPLOY_DIR / "docker-compose.whale-pair.yml"
        assert compose.is_file()

    def test_monitoring_doc_exists_and_mentions_stack(self) -> None:
        assert MONITOR_DOC_PATH.is_file(), f"missing monitoring doc: {MONITOR_DOC_PATH}"
        text = MONITOR_DOC_PATH.read_text()
        assert "Prometheus" in text
        assert "Grafana" in text
        assert "cron" in text.lower()
        assert "alert" in text.lower()
        assert "start_monitoring.sh" in text


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


class TestMonitoringComposeFile:
    def _load(self) -> dict:
        compose = DEPLOY_DIR / "docker-compose.monitoring.whale-pair.yml"
        assert compose.is_file(), f"missing monitoring compose file: {compose}"
        with compose.open() as fh:
            return yaml.safe_load(fh)

    def test_contains_monitoring_services(self) -> None:
        data = self._load()
        services = data.get("services", {})
        for name in ("prometheus", "grafana", "node-exporter", "cadvisor"):
            assert name in services

    def test_prometheus_mounts_repo_config_and_rules(self) -> None:
        svc = self._load()["services"]["prometheus"]
        volumes = svc.get("volumes", [])
        assert any("ops/prometheus/prometheus.whale-pair.yml" in item for item in volumes)
        assert any("ops/prometheus/rules" in item for item in volumes)

    def test_grafana_mounts_provisioning_and_dashboards(self) -> None:
        svc = self._load()["services"]["grafana"]
        volumes = svc.get("volumes", [])
        assert any("ops/grafana/provisioning" in item for item in volumes)
        assert any("ops/grafana/dashboards" in item for item in volumes)

    def test_binds_only_loopback_by_default(self) -> None:
        data = self._load()
        prom_ports = data["services"]["prometheus"].get("ports", [])
        grafana_ports = data["services"]["grafana"].get("ports", [])
        assert any("127.0.0.1" in port for port in prom_ports)
        assert any("127.0.0.1" in port for port in grafana_ports)


class TestMonitoringOpsFiles:
    def test_prometheus_config_exists_and_scrapes_targets(self) -> None:
        path = OPS_DIR / "prometheus" / "prometheus.whale-pair.yml"
        assert path.is_file()
        data = yaml.safe_load(path.read_text())
        jobs = {job["job_name"] for job in data.get("scrape_configs", [])}
        assert {"prometheus", "node-exporter", "cadvisor"} <= jobs
        assert data.get("rule_files"), "prometheus config must load alert rules"

    def test_alert_rules_exist_for_key_failures(self) -> None:
        path = OPS_DIR / "prometheus" / "rules" / "whale-pair-alerts.yml"
        assert path.is_file()
        data = yaml.safe_load(path.read_text())
        alerts = {
            rule["alert"]
            for group in data.get("groups", [])
            for rule in group.get("rules", [])
            if "alert" in rule
        }
        assert "WhalePairServiceDown" in alerts
        assert "WhalePairBookAgeStale" in alerts
        assert "WhalePairLedgerStale" in alerts

    def test_grafana_datasource_and_dashboard_provisioning_exist(self) -> None:
        ds = OPS_DIR / "grafana" / "provisioning" / "datasources" / "prometheus.yml"
        dashboards = OPS_DIR / "grafana" / "provisioning" / "dashboards" / "dashboards.yml"
        dashboard_json = OPS_DIR / "grafana" / "dashboards" / "whale-pair-overview.json"
        assert ds.is_file()
        assert dashboards.is_file()
        assert dashboard_json.is_file()

    def test_dashboard_json_mentions_core_metrics(self) -> None:
        path = OPS_DIR / "grafana" / "dashboards" / "whale-pair-overview.json"
        data = json.loads(path.read_text())
        assert data["title"] == "Whale Pair Overview"
        blob = path.read_text()
        assert "whale_pair_service_up" in blob
        assert "whale_pair_service_book_age_avg_ms" in blob
        assert "whale_pair_service_ledger_age_seconds" in blob


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

    @pytest.mark.parametrize("name", ["backup_data.sh", "restore_data.sh", "standby_bootstrap.sh"])
    def test_usage_scripts_print_help(self, name: str) -> None:
        result = subprocess.run(
            ["bash", str(DEPLOY_DIR / name), "--help"],
            capture_output=True,
            text=True,
        )
        assert result.returncode == 0
        assert "Usage" in result.stdout + result.stderr


class TestBackupAndStandbyHelpers:
    def test_backup_data_creates_archive_manifest_and_checksum(self, tmp_path: Path) -> None:
        data_dir = tmp_path / "data"
        data_dir.mkdir()
        (data_dir / "notes.txt").write_text("backup hello\n")
        conn = sqlite3.connect(str(data_dir / "whale_pair_live.db"))
        conn.execute("CREATE TABLE events (id INTEGER PRIMARY KEY, note TEXT)")
        conn.execute("INSERT INTO events (note) VALUES ('row-1')")
        conn.commit()
        conn.close()

        backups_dir = tmp_path / "backups"
        result = subprocess.run(
            [
                "bash",
                str(DEPLOY_DIR / "backup_data.sh"),
                "--dest",
                str(backups_dir),
                "--keep",
                "2",
                "--name",
                "pytest",
            ],
            capture_output=True,
            text=True,
            env={**os.environ, "DATA_DIR": str(data_dir)},
        )
        assert result.returncode == 0, result.stderr

        archives = list(backups_dir.glob("whale-pair-data-*.tar.gz"))
        manifests = list(backups_dir.glob("whale-pair-data-*.manifest.json"))
        checksums = list(backups_dir.glob("whale-pair-data-*.sha256"))
        assert len(archives) == len(manifests) == len(checksums) == 1
        assert "db count: 1" in result.stdout

    def test_restore_data_restores_latest_archive(self, tmp_path: Path) -> None:
        source_data = tmp_path / "source-data"
        source_data.mkdir()
        (source_data / "notes.txt").write_text("restored hello\n")
        conn = sqlite3.connect(str(source_data / "whale_pair_live.db"))
        conn.execute("CREATE TABLE events (id INTEGER PRIMARY KEY, note TEXT)")
        conn.execute("INSERT INTO events (note) VALUES ('restored-row')")
        conn.commit()
        conn.close()

        backups_dir = tmp_path / "backups"
        backup = subprocess.run(
            [
                "bash",
                str(DEPLOY_DIR / "backup_data.sh"),
                "--dest",
                str(backups_dir),
                "--keep",
                "2",
            ],
            capture_output=True,
            text=True,
            env={**os.environ, "DATA_DIR": str(source_data)},
        )
        assert backup.returncode == 0, backup.stderr

        target_data = tmp_path / "target-data"
        target_data.mkdir()
        (target_data / "old.txt").write_text("old\n")

        restore = subprocess.run(
            [
                "bash",
                str(DEPLOY_DIR / "restore_data.sh"),
                "--latest",
                "--backup-dir",
                str(backups_dir),
                "--target",
                str(target_data),
                "--force",
            ],
            capture_output=True,
            text=True,
        )
        assert restore.returncode == 0, restore.stderr
        assert (target_data / "notes.txt").read_text() == "restored hello\n"

        conn = sqlite3.connect(str(target_data / "whale_pair_live.db"))
        row = conn.execute("SELECT note FROM events").fetchone()
        conn.close()
        assert row == ("restored-row",)

    def test_standby_bootstrap_writes_role_file_without_starting(self, tmp_path: Path) -> None:
        data_dir = tmp_path / "standby-data"
        data_dir.mkdir()
        env_file = _write_env(tmp_path, POLYMARKET_SIGNATURE_TYPE="0", POLYMARKET_FUNDER="")

        result = subprocess.run(
            [
                "bash",
                str(DEPLOY_DIR / "standby_bootstrap.sh"),
                "--primary",
                "pytest-primary",
                "--no-start",
            ],
            capture_output=True,
            text=True,
            env={
                **os.environ,
                "DATA_DIR": str(data_dir),
                "BACKUP_DIR": str(data_dir / "backups"),
                "ENV_FILE": str(env_file),
            },
        )
        assert result.returncode == 0, result.stderr
        role_file = data_dir / "whale_pair_standby.role"
        assert role_file.is_file()
        text = role_file.read_text()
        assert "role=passive-standby" in text
        assert "primary=pytest-primary" in text


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

    def test_backup_doc_mentions_restore_and_data_dir(self) -> None:
        text = BACKUP_DOC_PATH.read_text().lower()
        assert "/opt/polymarket-agent/data" in text
        assert "backup_data.sh" in text
        assert "restore_data.sh" in text
        assert "--force" in text
        assert "standby" in text

    def test_standby_doc_mentions_bootstrap_status_and_promotion(self) -> None:
        text = STANDBY_DOC_PATH.read_text().lower()
        assert "standby_bootstrap.sh" in text
        assert "standby_status.sh" in text
        assert "dry-run" in text
        assert "--live" in text
