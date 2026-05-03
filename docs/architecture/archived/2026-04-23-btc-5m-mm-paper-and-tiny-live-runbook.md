# BTC 5m MM Paper And Tiny-Live Runbook

Status: operator runbook
Date: 2026-04-24
Primary runtime: `polymarket-exec`

## 1. Scope

This runbook covers:

- paper launches for the current sleeve set
- parallel paper launches on one host
- the exact persistent paths and health checks to verify
- the minimum operator path for tiny-live once the runtime is stable

It is intentionally limited to ops/docs/script surfaces. It does not approve
capital deployment by itself.

It assumes the repo lives at:

- `/Users/jackreid/go/polymarket-agent`

Adjust paths if the runtime is moved to a dedicated host.

## 1.1 Hard live gate

Live mode must not carry capital until order submission, cancel, replace, open
order sync, and balance sync go through the signed Polymarket CLOB API/SDK path.
Do not work around this with unsigned/raw HTTP calls or paper adapter semantics.

Tiny-live may only start after:

- `PM_BTC_5M_PAPER_MODE=false` is deliberate and reviewed
- signed CLOB credentials or L1 private-key auth are present and scoped to the tiny-live wallet
- the user websocket is authenticated and producing order/fill events
- startup reconciliation has been tested against the venue open-order source
- the first funded run uses one sleeve, one host, and the smallest practical size
- `PM_BTC_5M_EXEC_MODE=live_smoke` has completed submit, visible-open-order sync,
  cancel, and post-cancel sync before any strategy sleeve is allowed to trade

## 2. Available sleeve presets

Current checked-in sleeve env files:

- `polymarket-exec/env/unlawful_baseline.env`
- `polymarket-exec/env/unlawful_broad_hours.env`
- `polymarket-exec/env/unlawful_press.env`
- `polymarket-exec/env/goat_pair_baseline.env`

Each sleeve owns:

- a unique service name
- a unique metrics port
- a dedicated SQLite order-store path
- a dedicated JSONL journal path
- a dedicated JSONL audit path

The checked-in sleeve paths are relative to the Rust crate working directory
because the launcher starts the binary from `polymarket-exec/`. On the current
Hetzner host, `data/runtime/...` therefore resolves to
`/root/go/polymarket-agent/polymarket-exec/data/runtime/...`.

## 3. Default paper launcher

The generic operator entrypoint is:

```bash
polymarket-exec/scripts/run_sleeve.sh <sleeve-name>
```

Examples:

```bash
polymarket-exec/scripts/run_sleeve.sh unlawful_baseline
polymarket-exec/scripts/run_sleeve.sh unlawful_broad_hours
polymarket-exec/scripts/run_sleeve.sh unlawful_press
polymarket-exec/scripts/run_sleeve.sh goat_pair_baseline
```

What it does:

- resolves `polymarket-exec/env/<name>.env`
- exports `PM_BTC_5M_SLEEVE_ENV_PATH`
- calls `polymarket-exec/scripts/run_unlawful_shear_paper.sh`
- regenerates:
  - `data/research/wallet_research/unlawful-shear/rust_runtime.env`
  - `data/research/wallet_research/unlawful-shear/rust_market_context.json`
- re-exports the rolling BTC 5m slate every `45s` by default
- restarts the sleeve when the market slate changes so websocket subscriptions stay fresh
- starts the Rust runtime with `cargo run`

Override the refresh cadence with:

- `PM_BTC_5M_CONTEXT_REFRESH_INTERVAL_SEC=<n>`
- `PM_BTC_5M_CONTEXT_REFRESH_INTERVAL_SEC=0` to disable refresh supervision

The rolling refresh is a launcher/control-plane responsibility, not a strategy signal.
On an always-on host it should stay enabled so the sleeves keep rotating onto fresh
BTC 5m prev/current/next markets without operator intervention.

## 4. Parallel paper launch

Run each sleeve in its own terminal, tmux pane, or service unit.

Example:

```bash
polymarket-exec/scripts/run_sleeve.sh unlawful_baseline
polymarket-exec/scripts/run_sleeve.sh unlawful_broad_hours
polymarket-exec/scripts/run_sleeve.sh unlawful_press
```

Metrics ports:

- `unlawful_baseline`: `127.0.0.1:9108`
- `unlawful_broad_hours`: `127.0.0.1:9109`
- `unlawful_press`: `127.0.0.1:9110`
- `goat_pair_baseline`: `127.0.0.1:9111`

Persistent state:

- `polymarket-exec/data/runtime/unlawful-baseline/order-store.sqlite`
- `polymarket-exec/data/execution/paper/unlawful-baseline/journal.jsonl`
- `polymarket-exec/data/execution/audit/unlawful-baseline/audit.jsonl`
- `polymarket-exec/data/runtime/unlawful-broad-hours/order-store.sqlite`
- `polymarket-exec/data/execution/paper/unlawful-broad-hours/journal.jsonl`
- `polymarket-exec/data/execution/audit/unlawful-broad-hours/audit.jsonl`
- `polymarket-exec/data/runtime/unlawful-press/order-store.sqlite`
- `polymarket-exec/data/execution/paper/unlawful-press/journal.jsonl`
- `polymarket-exec/data/execution/audit/unlawful-press/audit.jsonl`
- `polymarket-exec/data/runtime/goat-baseline/order-store.sqlite`
- `polymarket-exec/data/execution/paper/goat-baseline/journal.jsonl`

## 5. Bootstrap-only validation

To verify the export/env/bootstrap path without starting Rust:

```bash
PM_BTC_5M_SKIP_CARGO_RUN=true polymarket-exec/scripts/run_sleeve.sh unlawful_baseline
```

For offline fallback:

```bash
PM_BTC_5M_CONTEXT_SOURCE=db \
PM_BTC_5M_SKIP_CARGO_RUN=true \
polymarket-exec/scripts/run_sleeve.sh unlawful_baseline
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

Heartbeat expectations:

- `/healthz` fails if the runtime snapshot is older than `120s`
- `/healthz` fails if market websocket messages are older than `30s`
- `/healthz` fails if active orders exist and reconcile age exceeds `120s`
- `/metrics` should expose `market_ws_last_message_age_ms`,
  `user_ws_last_message_age_ms`, and `last_reconcile_age_ms`
- paper services should append journal/checkpoint records at least every active
  runtime checkpoint interval, default `30s`, once a market slate is live

If `/healthz` is red, treat it as fail-closed for tiny-live. For paper, keep the
process running only if the failure is understood and the journal/order store are
still useful for diagnosis.

## 7. Restart procedure

Manual:

1. stop the process
2. re-run the same sleeve command
3. verify `healthz`, `api/state`, and journal append behavior

Startup reconciliation:

1. On startup, the runtime opens the configured SQLite order store.
2. Active rows older than `PM_BTC_5M_ORDER_RECONCILE_STALE_MS`, default
   `30000ms`, are moved toward `NeedsReconcile`.
3. A startup checkpoint is written to the journal.
4. In paper, confirm old `CancelRequested` rows do not remain stuck after restart.
5. In tiny-live, compare local active rows with venue open orders before allowing
   new risk. If any local/venue state is unknown, keep the sleeve stopped or in
   cleanup-only mode until reconciled.

systemd:

```bash
systemctl --user daemon-reload
systemctl --user enable polymarket-exec@unlawful_baseline
systemctl --user start polymarket-exec@unlawful_baseline
systemctl --user status polymarket-exec@unlawful_baseline
journalctl --user -u polymarket-exec@unlawful_baseline -f
```

Template file:

- `polymarket-exec/ops/systemd/polymarket-exec@.service`

Host install helpers:

- `polymarket-exec/ops/systemd/install_user_paper_services.sh`
- `polymarket-exec/ops/systemd/manage_unlawful_paper_services.sh`

Host env surfaces:

- `~/.config/polymarket-exec/common.env`
- `~/.config/polymarket-exec/paper.d/unlawful_baseline.env`
- `~/.config/polymarket-exec/paper.d/unlawful_broad_hours.env`
- `~/.config/polymarket-exec/paper.d/unlawful_press.env`

The systemd template reads those env files if present, then executes the checked-in
repo launcher. That means repo updates remain the source of truth for the sleeves,
while host env files only carry deployment-specific overrides.

## 8. Hetzner paper service model

Current direct rollout entrypoint:

```bash
HETZNER_HOST=<host-or-ip> ops/deploy/deploy_paper_hetzner.sh
```

Default remote surfaces:

- SSH key: `~/.ssh/polymarket_hetzner`
- remote user: `root`
- remote checkout: `/root/go/polymarket-agent`
- managed services:
  - `polymarket-exec@unlawful_baseline`
  - `polymarket-exec@unlawful_broad_hours`
  - `polymarket-exec@unlawful_press`

The deploy script syncs the repo, installs user services, enables lingering,
starts the three paper sleeves, and probes ports `9108`, `9109`, and `9110`.
It excludes local `data/`, `target/`, `.git/`, `.venv/`, and runtime artifacts.

Hetzner paper is acceptable for continuous paper service operation. For real
capital, prefer the EU live plan below rather than treating the current Hetzner
paper host as the permanent live trading plane.

## 9. Paper env and secret surfaces

Paper mode still needs a small number of explicit host surfaces:

- `PM_BTC_5M_ROOT_DIR`
- `PM_BTC_5M_CONTEXT_SOURCE`
- `PM_BTC_5M_INCLUDE_PREV`
- `PM_BTC_5M_INCLUDE_NEXT`
- `PM_BTC_5M_CONTEXT_REFRESH_INTERVAL_SEC`
- `PM_BTC_5M_PYTHON_BIN`
- `PM_BTC_5M_CARGO_BIN`
- `PM_BTC_5M_EXEC_SPOT_WS_URL`
- `PM_BTC_5M_EXEC_SPOT_SYMBOL`
- `POLYMARKET_MARKET_WS_URL`
- `POLYMARKET_USER_WS_URL`
- `PM_BTC_5M_ORDER_RECONCILE_INTERVAL_MS`
- `PM_BTC_5M_ORDER_RECONCILE_STALE_MS`
- `PM_BTC_5M_RUNTIME_CHECKPOINT_INTERVAL_MS`
- `PM_BTC_5M_EXEC_JOURNAL_ROTATE_BYTES`
- `RUST_LOG`
- `RUST_BACKTRACE`

The rolling exporter generates these at launch:

- `PM_BTC_5M_ASSET_IDS`
- `PM_BTC_5M_INSTRUMENT_MARKETS`
- `PM_BTC_5M_USER_MARKETS`
- `PM_BTC_5M_LATEST_MARKET_SLUG`
- `PM_BTC_5M_LATEST_MARKET_END_TIME`

Paper mode does not require:

- `POLYMARKET_API_KEY`
- `POLYMARKET_API_SECRET`
- `POLYMARKET_API_PASSPHRASE`
- `POLYMARKET_PRIVATE_KEY`
- `POLYMARKET_SIGNATURE_TYPE`
- `POLYMARKET_FUNDER_ADDRESS`

Journal and state paths remain sleeve-local and come from the checked-in env files:

- `polymarket-exec/data/runtime/unlawful-baseline/order-store.sqlite`
- `polymarket-exec/data/execution/paper/unlawful-baseline/journal.jsonl`
- `polymarket-exec/data/execution/audit/unlawful-baseline/audit.jsonl`
- `polymarket-exec/data/runtime/unlawful-broad-hours/order-store.sqlite`
- `polymarket-exec/data/execution/paper/unlawful-broad-hours/journal.jsonl`
- `polymarket-exec/data/execution/audit/unlawful-broad-hours/audit.jsonl`
- `polymarket-exec/data/runtime/unlawful-press/order-store.sqlite`
- `polymarket-exec/data/execution/paper/unlawful-press/journal.jsonl`
- `polymarket-exec/data/execution/audit/unlawful-press/audit.jsonl`

Process logs for always-on runs live in journald:

- `journalctl --user -u polymarket-exec@unlawful_baseline`
- `journalctl --user -u polymarket-exec@unlawful_broad_hours`
- `journalctl --user -u polymarket-exec@unlawful_press`

## 10. Data retention and off-disk storage

Hot state stays local and sleeve-specific:

- SQLite order stores under `data/runtime/*/order-store.sqlite`
- active journals under `data/execution/paper/*/journal.jsonl`
- rotated journal segments under the same paper directories

Off-disk policy:

- enable `PM_BTC_5M_EXEC_JOURNAL_ROTATE_BYTES=268435456`
- archive rotated journal segments and explicit backup directories to S3
- use `DEEP_ARCHIVE` for cold paper logs unless active replay access is needed
- do not delete local active `journal.jsonl`
- only enable `PM_BTC_5M_ARCHIVE_DELETE_LOCAL_AFTER_UPLOAD=true` after S3
  object existence has been verified

Run a local candidate listing before trusting credentials:

```bash
PM_BTC_5M_ARCHIVE_LIST_ONLY=true polymarket-exec/scripts/archive_paper_artifacts.sh
```

Detailed archive procedure:

- `docs/architecture/2026-04-24-paper-storage-archive-plan.md`

## 11. EU/live considerations

Paper can run on local or Hetzner as long as websockets are stable and retention
is configured.

Tiny-live/live should use a Europe-based trading plane, with `eu-west-1` as the
default AWS target when moving beyond paper. The control plane and research
collector can lag behind; the trading plane needs the lowest operational
uncertainty for websocket, CLOB, RPC, and reconciliation surfaces.

Do not run meaningful capital from a host where:

- user websocket auth is untested
- Polygon RPC is missing or rate-limited
- signed CLOB API/SDK execution is not the only live order path
- order-store persistence is on ephemeral disk without a tested backup path
- archive/retention is disabled and active journals are unbounded

## 12. Tiny-live minimum env additions

Paper mode leaves these unset.

Before tiny-live, fill:

- `POLYMARKET_PRIVATE_KEY`
- `POLYMARKET_SIGNATURE_TYPE=eoa` and no `POLYMARKET_FUNDER_ADDRESS` when the
  funded account is the MetaMask signer address itself
- `POLYMARKET_SIGNATURE_TYPE=gnosis_safe` and
  `POLYMARKET_FUNDER_ADDRESS=<Polymarket Safe/proxy wallet address>` only when
  the funded account is a Safe/proxy wallet
- `POLYMARKET_API_KEY`, `POLYMARKET_API_SECRET`, and
  `POLYMARKET_API_PASSPHRASE` if you want the user websocket enabled from
  process start
- `PM_BTC_5M_PAPER_MODE=false`
- `POLYMARKET_MARKET_WS_URL`
- `POLYMARKET_USER_WS_URL`
- `PM_BTC_5M_EXEC_SPOT_WS_URL`
- `PM_BTC_5M_EXEC_SPOT_SYMBOL`
- `PM_BTC_5M_ORDER_RECONCILE_INTERVAL_MS`
- `PM_BTC_5M_ORDER_RECONCILE_STALE_MS`

And verify:

- the live wallet is distinct from the research wallet
- direct EOA live auth is used only for smoke/tiny-live and not for scaling
- the live wallet is a Gnosis Safe / proxy wallet before scaling beyond smoke
- approvals and balances are already staged
- the user websocket is receiving live order / fill events
- signed CLOB API/SDK submit/cancel/open-order sync is confirmed before strategy capital
- startup reconciliation agrees with venue open orders before new risk is allowed

Live-auth model:

- Polymarket public market data and CLOB read endpoints do not need auth.
- CLOB trading requires L1 private-key signing plus L2 API authentication.
- The Rust SDK path can derive/create L2 credentials from `POLYMARKET_PRIVATE_KEY`
  at startup. This is acceptable for REST submit/cancel/open-order sync.
- The user websocket should still be given explicit `POLYMARKET_API_KEY`,
  `POLYMARKET_API_SECRET`, and `POLYMARKET_API_PASSPHRASE`; otherwise live fill
  tracking falls back to polling/reconciliation and the sleeve should remain in
  smoke or paper.
- `METAMASK_PRIVATE_KEY` is accepted only as a local fallback alias when
  `POLYMARKET_PRIVATE_KEY` is unset. Do not set both to different wallets.

Gnosis Safe setup rules:

- Use the private key for the signer that controls the Polymarket account.
- Set `POLYMARKET_SIGNATURE_TYPE=gnosis_safe`.
- Set `POLYMARKET_FUNDER_ADDRESS` to the Polymarket Safe/proxy address that
  holds USDC and conditional-token positions.
- Confirm the same funder address appears in Polymarket profile / account
  state before enabling live.
- Do not reuse the research whale wallet, collector wallet, or dashboard wallet
  as the live signer/funder.

Validated smoke configuration:

- On `2026-04-24`, live smoke succeeded on AWS using the funded MetaMask EOA
  path.
- `POLYMARKET_PRIVATE_KEY` was set from the MetaMask private key.
- `POLYMARKET_SIGNATURE_TYPE=eoa`.
- `POLYMARKET_FUNDER_ADDRESS` was unset.
- The one-shot placed a `$1` far-touch order, cancelled it, and reconciled open
  orders successfully.

Live smoke mode:

```bash
PM_BTC_5M_EXEC_MODE=live_smoke \
PM_BTC_5M_PAPER_MODE=false \
PM_BTC_5M_LIVE_SMOKE_ASSET_ID=<outcome-token-id> \
PM_BTC_5M_LIVE_SMOKE_MARKET_ID=<market-id> \
PM_BTC_5M_LIVE_SMOKE_PRICE=0.01 \
PM_BTC_5M_LIVE_SMOKE_NOTIONAL_USD=1.0 \
cargo run -p polymarket-exec
```

Success criteria:

- one post-only GTD order is accepted
- the order appears in CLOB open-order sync
- cancel is accepted
- the order no longer appears after post-cancel sync
- no local order remains `Open`, `CancelRequested`, or `NeedsReconcile`

## 13. Tiny-live checklist

Do not start tiny-live until all of these are true:

- the crate builds cleanly
- the target sleeve survives multi-hour paper runs
- the runtime reconnects cleanly after websocket drops
- order-store and journal recovery are verified on restart
- market slate export is working from `--source live`
- metrics and health endpoints are reachable
- microstructure controller is enabled and visible in decision notes
- `PM_BTC_5M_UNLAWFUL_SHEAR_MICROSTRUCTURE_REQUIRE_DEPTH=true` is set for live
- signed CLOB API/SDK is the only live order submission/cancel path
- `PM_BTC_5M_EXEC_MODE=live_smoke` has passed on the live host
- startup reconciliation against venue open orders is manually verified
- top-3 ask notional caps are tighter than max order notional
- stale `CancelRequested` orders transition to `NeedsReconcile`
- operator kill path has been tested with `systemctl --user stop polymarket-exec@<sleeve>`

First tiny-live run should be:

- one sleeve only
- one host only
- smallest practical size
- manual observation on logs and metrics
- zero auto-restart escalation after a kill until the order store is reconciled

Live kill/health loop:

1. Watch `/healthz`, `/api/state`, systemd status, journal append rate, and order-store active orders.
2. Kill immediately if user websocket is stale, orders remain `CancelRequested` past the reconciliation window, depth telemetry disappears while `MICROSTRUCTURE_REQUIRE_DEPTH=true`, or inventory exceeds the configured hard cap.
3. After a kill, reconcile venue open orders first, then restart the sleeve only after `orders` has no unknown active rows.

## 14. Collector relationship

The runtime and the unlawful live collector are separate processes.

The runtime should not wait on the collector.

The collector should be run alongside paper so:

- live unlawful timing can be rechecked
- session drift can be measured
- geometry thresholds can be recalibrated

## 15. Concise AWS paper checklist

An always-on paper deploy should be:

1. Launch an EU host if available, preferably `eu-west-1`.
2. Clone the repo to the final path, for example:
   - `$HOME/go/polymarket-agent`
3. Install Rust, Python 3, and systemd user-service support.
4. Run:

```bash
polymarket-exec/ops/systemd/install_user_paper_services.sh
```

5. Edit `~/.config/polymarket-exec/common.env`:
   - set `PM_BTC_5M_ROOT_DIR`
   - confirm BTC spot WS URL and symbol
   - keep `PM_BTC_5M_CONTEXT_SOURCE=live`
6. Enable lingering:

```bash
sudo loginctl enable-linger "$USER"
```

7. Reload and start the three unlawful services:

```bash
systemctl --user daemon-reload
systemctl --user enable --now polymarket-exec@unlawful_baseline
systemctl --user enable --now polymarket-exec@unlawful_broad_hours
systemctl --user enable --now polymarket-exec@unlawful_press
```

8. Verify:
   - `systemctl --user status polymarket-exec@unlawful_baseline`
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
