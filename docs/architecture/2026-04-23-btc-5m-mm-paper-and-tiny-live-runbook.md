# BTC 5m MM Paper And Tiny-Live Runbook

Status: operator runbook  
Date: 2026-04-23  
Primary runtime: `whale-pair-exec`

## 1. Scope

This runbook covers:

- paper launches for the current sleeve set
- parallel paper launches on one host
- the exact persistent paths and health checks to verify
- the minimum operator path for tiny-live once the runtime is stable

It assumes the repo lives at:

- `/Users/jackreid/go/polymarket-agent`

Adjust paths if the runtime is moved to a dedicated host.

## 2. Available sleeve presets

Current checked-in sleeve env files:

- `whale-pair-exec/env/unlawful_baseline.env`
- `whale-pair-exec/env/unlawful_broad_hours.env`
- `whale-pair-exec/env/unlawful_press.env`
- `whale-pair-exec/env/goat_pair_baseline.env`

Each sleeve owns:

- a unique service name
- a unique metrics port
- a dedicated SQLite order-store path
- a dedicated JSONL journal path

## 3. Default paper launcher

The generic operator entrypoint is:

```bash
whale-pair-exec/scripts/run_sleeve.sh <sleeve-name>
```

Examples:

```bash
whale-pair-exec/scripts/run_sleeve.sh unlawful_baseline
whale-pair-exec/scripts/run_sleeve.sh unlawful_broad_hours
whale-pair-exec/scripts/run_sleeve.sh unlawful_press
whale-pair-exec/scripts/run_sleeve.sh goat_pair_baseline
```

What it does:

- resolves `whale-pair-exec/env/<name>.env`
- exports `WHALE_PAIR_SLEEVE_ENV_PATH`
- calls `whale-pair-exec/scripts/run_unlawful_shear_paper.sh`
- regenerates:
  - `data/research/wallet_research/unlawful-shear/rust_runtime.env`
  - `data/research/wallet_research/unlawful-shear/rust_market_context.json`
- re-exports the rolling BTC 5m slate every `45s` by default
- restarts the sleeve when the market slate changes so websocket subscriptions stay fresh
- starts the Rust runtime with `cargo run`

Override the refresh cadence with:

- `WHALE_PAIR_CONTEXT_REFRESH_INTERVAL_SEC=<n>`
- `WHALE_PAIR_CONTEXT_REFRESH_INTERVAL_SEC=0` to disable refresh supervision

The rolling refresh is a launcher/control-plane responsibility, not a strategy signal.
On an always-on host it should stay enabled so the sleeves keep rotating onto fresh
BTC 5m prev/current/next markets without operator intervention.

## 4. Parallel paper launch

Run each sleeve in its own terminal, tmux pane, or service unit.

Example:

```bash
whale-pair-exec/scripts/run_sleeve.sh unlawful_baseline
whale-pair-exec/scripts/run_sleeve.sh unlawful_broad_hours
whale-pair-exec/scripts/run_sleeve.sh unlawful_press
```

Metrics ports:

- `unlawful_baseline`: `127.0.0.1:9108`
- `unlawful_broad_hours`: `127.0.0.1:9109`
- `unlawful_press`: `127.0.0.1:9110`
- `goat_pair_baseline`: `127.0.0.1:9111`

Persistent state:

- `data/runtime/unlawful-baseline/order-store.sqlite`
- `data/execution/paper/unlawful-baseline/journal.jsonl`
- `data/runtime/unlawful-broad-hours/order-store.sqlite`
- `data/execution/paper/unlawful-broad-hours/journal.jsonl`
- `data/runtime/unlawful-press/order-store.sqlite`
- `data/execution/paper/unlawful-press/journal.jsonl`
- `data/runtime/goat-baseline/order-store.sqlite`
- `data/execution/paper/goat-baseline/journal.jsonl`

## 5. Bootstrap-only validation

To verify the export/env/bootstrap path without starting Rust:

```bash
WHALE_PAIR_SKIP_CARGO_RUN=true whale-pair-exec/scripts/run_sleeve.sh unlawful_baseline
```

For offline fallback:

```bash
WHALE_PAIR_CONTEXT_SOURCE=db \
WHALE_PAIR_SKIP_CARGO_RUN=true \
whale-pair-exec/scripts/run_sleeve.sh unlawful_baseline
```

## 6. Health checks

The HTTP ops surface exposes:

- `/healthz`
- `/metrics`
- `/api/state`
- `/api/whale/events`

Examples:

```bash
curl -sS http://127.0.0.1:9108/healthz
curl -sS http://127.0.0.1:9108/api/state
curl -sS http://127.0.0.1:9108/metrics
```

Expected paper checks:

- `healthz` returns success
- `api/state` shows the intended strategy and market slate
- the journal file is being appended to
- the order-store SQLite file exists and grows

## 7. Restart procedure

Manual:

1. stop the process
2. re-run the same sleeve command
3. verify `healthz`, `api/state`, and journal append behavior

systemd:

```bash
systemctl --user daemon-reload
systemctl --user enable whale-pair-exec@unlawful_baseline
systemctl --user start whale-pair-exec@unlawful_baseline
systemctl --user status whale-pair-exec@unlawful_baseline
journalctl --user -u whale-pair-exec@unlawful_baseline -f
```

Template file:

- `whale-pair-exec/ops/systemd/whale-pair-exec@.service`

Host install helpers:

- `whale-pair-exec/ops/systemd/install_user_paper_services.sh`
- `whale-pair-exec/ops/systemd/manage_unlawful_paper_services.sh`

Host env surfaces:

- `~/.config/whale-pair-exec/common.env`
- `~/.config/whale-pair-exec/paper.d/unlawful_baseline.env`
- `~/.config/whale-pair-exec/paper.d/unlawful_broad_hours.env`
- `~/.config/whale-pair-exec/paper.d/unlawful_press.env`

The systemd template reads those env files if present, then executes the checked-in
repo launcher. That means repo updates remain the source of truth for the sleeves,
while host env files only carry deployment-specific overrides.

## 8. Paper env and secret surfaces

Paper mode still needs a small number of explicit host surfaces:

- `WHALE_PAIR_ROOT_DIR`
- `WHALE_PAIR_CONTEXT_SOURCE`
- `WHALE_PAIR_INCLUDE_PREV`
- `WHALE_PAIR_INCLUDE_NEXT`
- `WHALE_PAIR_CONTEXT_REFRESH_INTERVAL_SEC`
- `WHALE_PAIR_PYTHON_BIN`
- `WHALE_PAIR_CARGO_BIN`
- `WHALE_PAIR_EXEC_SPOT_WS_URL`
- `WHALE_PAIR_EXEC_SPOT_SYMBOL`
- `POLYMARKET_MARKET_WS_URL`
- `POLYMARKET_USER_WS_URL`
- `RUST_LOG`
- `RUST_BACKTRACE`

Paper mode does not require:

- `POLYMARKET_API_KEY`
- `POLYMARKET_API_SECRET`
- `POLYMARKET_API_PASSPHRASE`

Journal and state paths remain sleeve-local and come from the checked-in env files:

- `data/runtime/unlawful-baseline/order-store.sqlite`
- `data/execution/paper/unlawful-baseline/journal.jsonl`
- `data/runtime/unlawful-broad-hours/order-store.sqlite`
- `data/execution/paper/unlawful-broad-hours/journal.jsonl`
- `data/runtime/unlawful-press/order-store.sqlite`
- `data/execution/paper/unlawful-press/journal.jsonl`

Process logs for always-on runs live in journald:

- `journalctl --user -u whale-pair-exec@unlawful_baseline`
- `journalctl --user -u whale-pair-exec@unlawful_broad_hours`
- `journalctl --user -u whale-pair-exec@unlawful_press`

## 9. Tiny-live minimum env additions

Paper mode leaves these unset.

Before tiny-live, fill:

- `POLYMARKET_API_KEY`
- `POLYMARKET_API_SECRET`
- `POLYMARKET_API_PASSPHRASE`
- `WHALE_PAIR_PAPER_MODE=false`
- `WHALE_PAIR_EXEC_SPOT_WS_URL`
- `WHALE_PAIR_EXEC_SPOT_SYMBOL`

And verify:

- the live wallet is distinct from the research wallet
- approvals and balances are already staged
- the user websocket is receiving live order / fill events

## 10. Tiny-live checklist

Do not start tiny-live until all of these are true:

- the crate builds cleanly
- the target sleeve survives multi-hour paper runs
- the runtime reconnects cleanly after websocket drops
- order-store and journal recovery are verified on restart
- market slate export is working from `--source live`
- metrics and health endpoints are reachable

First tiny-live run should be:

- one sleeve only
- one host only
- smallest practical size
- manual observation on logs and metrics

## 11. Collector relationship

The runtime and the unlawful live collector are separate processes.

The runtime should not wait on the collector.

The collector should be run alongside paper so:

- live unlawful timing can be rechecked
- session drift can be measured
- geometry thresholds can be recalibrated

## 12. Concise AWS paper checklist

Tomorrow’s always-on paper deploy should be:

1. Launch an EU host if available, preferably `eu-west-1`.
2. Clone the repo to the final path, for example:
   - `$HOME/go/polymarket-agent`
3. Install Rust, Python 3, and systemd user-service support.
4. Run:

```bash
whale-pair-exec/ops/systemd/install_user_paper_services.sh
```

5. Edit `~/.config/whale-pair-exec/common.env`:
   - set `WHALE_PAIR_ROOT_DIR`
   - confirm BTC spot WS URL and symbol
   - keep `WHALE_PAIR_CONTEXT_SOURCE=live`
6. Enable lingering:

```bash
sudo loginctl enable-linger "$USER"
```

7. Reload and start the three unlawful services:

```bash
systemctl --user daemon-reload
systemctl --user enable --now whale-pair-exec@unlawful_baseline
systemctl --user enable --now whale-pair-exec@unlawful_broad_hours
systemctl --user enable --now whale-pair-exec@unlawful_press
```

8. Verify:
   - `systemctl --user status whale-pair-exec@unlawful_baseline`
   - `curl -sS http://127.0.0.1:9108/healthz`
   - `curl -sS http://127.0.0.1:9109/healthz`
   - `curl -sS http://127.0.0.1:9110/healthz`
   - journals are appending under `data/execution/paper/*/journal.jsonl`
   - SQLite stores exist under `data/runtime/*/order-store.sqlite`

For a direct Hetzner-style rollout from a local checkout, use:

```bash
HETZNER_HOST=<current-host-or-ip> \
ops/deploy/deploy_paper_hetzner.sh
```
