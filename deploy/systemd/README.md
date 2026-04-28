# Systemd units for shadowlive + live bonereaper

Two systemd units that run on the AWS Dublin bot:

- `polymarket-shadow.service` — runs the engine in `WHALE_PAIR_EXEC_MODE=shadow_live`
  with `paper_mode=true`. Connects to live market WS + spot WS, runs bonereaper,
  routes every submit through the paper adapter. Writes journal +
  book snapshots to `/var/lib/polymarket/shadow_live_bonereaper/`.
- `polymarket-bonereaper-live.service` — runs the engine in
  `WHALE_PAIR_EXEC_MODE=live` at micro-capital ($5 clip × 50 markets =
  $250 max entry exposure). Reads the shadow process's journal at the
  fidelity gate; refuses strategy entries unless shadow has emitted an
  OK verdict in the last 24h. Writes its own journal to
  `/var/lib/polymarket/live_bonereaper/`.

The two are deliberately decoupled: live is `After=` shadow but not
`Requires=` it. If shadow restarts, live stays up and gates to
`RiskOff` until shadow's journal is fresh again. Hard-linking would
cause crash loops; graceful RiskOff is the safer default.

## Install

```bash
# On the AWS Dublin box (ec2-3-252-32-40.eu-west-1.compute.amazonaws.com):
ssh -i ~/.ssh/whale_pair_dublin_ed25519.pem ubuntu@ec2-3-252-32-40.eu-west-1.compute.amazonaws.com

# Pull the worktree:
cd ~/polymarket-agent
git fetch origin shadowlive-fidelity-bonereaper
git checkout shadowlive-fidelity-bonereaper
cargo build --release -p polymarket-exec

# Install the units:
sudo cp deploy/systemd/polymarket-shadow.service /etc/systemd/system/
sudo cp deploy/systemd/polymarket-bonereaper-live.service /etc/systemd/system/
sudo systemctl daemon-reload

# Start shadow only first. Let it run for at least 24h before enabling
# live (per the fidelity gate window).
sudo systemctl enable --now polymarket-shadow
sudo journalctl -u polymarket-shadow -f
```

## Promote shadow to live

After ≥ 24h of shadow running cleanly:

```bash
# From the local box:
python3 polymarket-exec/scripts/bonereaper_exec_compare.py \
    --mode fidelity-trend \
    --journal-log /var/lib/polymarket/shadow_live_bonereaper/journal.jsonl
```

If all market families show `fail=0` and `avg_mape < 0.30` over the
window, enable the live unit:

```bash
sudo systemctl enable --now polymarket-bonereaper-live
sudo journalctl -u polymarket-bonereaper-live -f
```

The first 60-90 seconds of live will report `RuntimeStatus::RiskOff`
with reason `shadow_unobserved` (the gate is conservative). Once the
gate confirms an in-window OK verdict, live will transition to
`Running` and the strategy will start emitting intents.

## Kill switch

```bash
# Halt live immediately, keep shadow running:
sudo systemctl stop polymarket-bonereaper-live

# Or use the file-based kill switch (no sudo needed):
touch /var/lib/polymarket/live.kill
```

The file-based kill switch is safer because the engine checks it on
every loop tick; systemd stop is a SIGTERM that may take seconds to
propagate.

## Logs

- **Shadow:** `journalctl -u polymarket-shadow -f`
- **Live:**   `journalctl -u polymarket-bonereaper-live -f`
- **Journal files** for post-hoc analysis:
  - Shadow: `/var/lib/polymarket/shadow_live_bonereaper/journal.jsonl`
  - Live:   `/var/lib/polymarket/live_bonereaper/journal.jsonl`

## Resource caps

Both units cap at `MemoryMax=1G` and `CPUQuota=50%`. With three
processes on the box (tinylive + shadow + bonereaper-live), total
budget is 1.5 cores and 3GB. Adjust if `journalctl` shows OOMKills.
