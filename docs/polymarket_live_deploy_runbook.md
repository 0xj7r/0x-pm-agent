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
2. Get the host onto that exact `main` commit or rsync the source patch as a temporary incident workaround.
3. Build the binary on the Linux host.
4. Install to a versioned binary path, for example `~/.local/bin/polymarket-exec-<sha>`.
5. Point `~/.local/bin/polymarket-exec` at the new versioned binary.
6. Restart the service only when allowed for the current live market.
7. Verify `systemctl --user is-active`, `readlink -f`, runtime state, and live logs.

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
