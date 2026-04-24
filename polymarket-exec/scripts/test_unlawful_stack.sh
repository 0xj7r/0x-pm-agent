#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT_DIR"

echo "[unlawful-test] focused gate scenarios"
cargo test -p polymarket-exec unlawful_gate -- --nocapture

echo "[unlawful-test] config regression"
cargo test -p polymarket-exec unlawful_shear_from_env_respects_offhour_override_flag -- --nocapture

echo "[unlawful-test] structured eval logging"
cargo test -p polymarket-exec unlawful_shear_signal_reason_and_aggression_logged_in_decision_notes -- --nocapture

echo "[unlawful-test] runtime scenario harness"
cargo test -p polymarket-exec --test btc_5m_mm_scenarios -- --nocapture

echo "[unlawful-test] calibration exporter syntax"
python3 -m py_compile scripts/export_unlawful_whale_vs_us.py

echo "[unlawful-test] ok"
