# Polymarket live deploy runbook

This is the operational checklist for `polymarket-exec` tinylive deploys.

## Golden rule

Do not deploy a local macOS `target/release/polymarket-exec` binary to the
Linux host. It will fail with:

```text
cannot execute binary file: Exec format error
```

Build on the target Linux host, or deploy a Linux CI artifact. A local macOS
build is only a compile check.

## Host/service

- Host: `ubuntu@34.242.101.97`
- SSH key: `~/.ssh/whale_pair_dublin_ed25519.pem`
- Service: `polymarket-exec@bonereaper_tinylive.service`
- Runtime dir: `/home/ubuntu/go/polymarket-agent`
- Binary symlink: `/home/ubuntu/.local/bin/polymarket-exec`
- Kill switch: `~/.config/polymarket-exec/live.kill`
- Journal: `/home/ubuntu/go/polymarket-agent/data/execution/live/btc-5m-bonereaper-tinylive/journal.jsonl`

## Safe deploy sequence

1. Commit intended code to `main` locally.
2. Use the first-class deploy command below. Do not hand-write SSH build wrappers.
3. Restart the service only when allowed for the current live market.
4. Verify `systemctl --user is-active`, `readlink -f`, runtime state, and live logs.

## First-class Bonereaper tinylive deploy

Run from the repo root after the intended code is committed and pushed to
`main`:

```bash
AWS_LIVE_HOST=34.242.101.97 \
AWS_LIVE_KEY_PATH=~/.ssh/whale_pair_dublin_ed25519.pem \
AWS_LIVE_REF=origin/main \
AWS_LIVE_SLEEVE=bonereaper_tinylive \
AWS_LIVE_SKIP_RESTART=0 \
ops/deploy/deploy_live_aws_ec2.sh
```

This command:

- Creates a clean git archive from the requested ref.
- Builds `polymarket-exec` on the Linux host through the host login shell.
- Installs a versioned host binary.
- Updates `/home/ubuntu/.local/bin/polymarket-exec`.
- Writes `/home/ubuntu/go/polymarket-agent/.deploy_commit`.
- Restarts `polymarket-exec@bonereaper_tinylive.service`.

If the service should not restart during the current live market, set
`AWS_LIVE_SKIP_RESTART=1`, then restart manually at the market boundary.

## Required verification commands

```bash
ssh -i ~/.ssh/whale_pair_dublin_ed25519.pem ubuntu@34.242.101.97 \
  'systemctl --user is-active polymarket-exec@bonereaper_tinylive.service; \
   readlink -f ~/.local/bin/polymarket-exec; \
   file "$(readlink -f ~/.local/bin/polymarket-exec)"'
```

```bash
ssh -i ~/.ssh/whale_pair_dublin_ed25519.pem ubuntu@34.242.101.97 \
  'sqlite3 /home/ubuntu/go/polymarket-agent/data/runtime/btc-5m-bonereaper-tinylive/order-store.sqlite \
   "select * from runtime_state;"'
```

```bash
ssh -i ~/.ssh/whale_pair_dublin_ed25519.pem ubuntu@34.242.101.97 \
  'journalctl --user -u polymarket-exec@bonereaper_tinylive.service --since "2 minutes ago" --no-pager -n 200'
```

## Known failure modes

- `Exec format error`: deployed a non-Linux binary. Restore previous versioned binary symlink immediately.
- `RiskOff` persists after DB update: the running process has `RiskOff` in memory. Stop/kill, update durable state while stopped, then start.
- Service stuck in `activating`: old process did not terminate. Use `systemctl --user kill -s SIGKILL ...`, then `reset-failed`, then start.
- No visible late-favorite orders despite `late_favorite climb` logs: quote reconciliation or repair mode may be crowding late-fav intents. Check order-store tags and `ladder pipeline counts`.
- Tiny `5.0` share merges despite `merge_min_qty=15`: runtime auto-merge bypassed strategy merge sizing. Runtime merge threshold must be aligned to profile/config.

## Current strategy-specific deploy checks

- Late-favorite must count live open late-fav orders against `max_load_usd`.
- Late-favorite cap must count only net favorite residual, not absolute side imbalance.
- Late-favorite quote priority must beat paired-core priority during quote reconciliation.
- Automatic merge should only ingest paired-core fills into merge accounting; late-favorite directional fills should not be recycled unless a separate cleanup rule explicitly chooses that.
