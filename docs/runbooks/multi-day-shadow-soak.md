# Multi-day shadow soak before live deployment

**Why:** the fidelity gate is the only thing standing between us and
naked-capital deployment of an unvalidated strategy. The gate logic has
unit tests but has not yet been exercised end-to-end against a real
multi-day shadow run. Before any live capital, run shadow for ≥ 48h
and verify the gate behaves as expected across at least one quiet
trading window (low fill volume), one busy window (high fill volume),
and one connectivity blip (WS reconnect, stale `/activity` truth).

Per memory `feedback_optimize_for_live_validation` and the spec
"Out of scope: Multi-day shadow-on-AWS run before any live capital."
This runbook executes that step.

## Prerequisites

- AWS Dublin bot (ec2-3-252-32-40.eu-west-1.compute.amazonaws.com) up and reachable.
- `polymarket-shadow.service` installed per `deploy/systemd/README.md`.
- `polymarket-bonereaper-live.service` installed but **NOT enabled**.
- Local shell with the SSH key: `~/.ssh/whale_pair_dublin_ed25519.pem`.

## Day 0: kick off

```bash
ssh -i ~/.ssh/whale_pair_dublin_ed25519.pem ubuntu@ec2-3-252-32-40.eu-west-1.compute.amazonaws.com 'sudo systemctl start polymarket-shadow && sudo systemctl status polymarket-shadow --no-pager'
```

Confirm:
- `Active: active (running)`
- First few log lines from the engine: `clob_v2_exchange detected`, market discovery began, WS connected.

## Day 0 + 6h: warmup gate

The queue model needs ≥ 8 observed fills per market family before it leaves NaN. Bonereaper does ~990 fills over its comparator window across 5 families ≈ ~200 per family per day. Should warm up within hours.

```bash
ssh ... 'tail -200 /var/lib/polymarket/shadow_live_bonereaper/journal.jsonl | grep queue_model_estimate'
```

Expected: per-family `queue_decay_rate_per_sec` rows that are no longer NaN, with `n_observations >= 8`.

If still NaN after 6h: investigate. Likely causes: market discovery failed, WS not connected, or strategy emitting zero intents (regime gate misfire).

## Day 1: first 24h gate check

```bash
ssh ... 'cat /var/lib/polymarket/shadow_live_bonereaper/journal.jsonl' \
  | python3 polymarket-exec/scripts/bonereaper_exec_compare.py \
        --mode fidelity-trend \
        --journal-log /dev/stdin
```

(Or scp the journal locally and run against the file.)

Decision matrix:

| Output | Action |
|---|---|
| All families `fail=0`, `avg_mape < 0.30` | Continue soak; gate is healthy. |
| Any family `fail > 0` | Stop. Diagnose with `--mode queue-decay-trend` + `--mode compare`. |
| `n` very small per family (e.g., `n < 10`) | Continue soak; not enough data yet. |
| Empty / missing rows | Investigate journal write path. |

## Days 2-3: catch failure modes

Deliberately exercise the gate's failure paths before promoting to live:

### a) Truth-source staleness

Block the AWS box from reaching `data-api.polymarket.com` briefly:

```bash
ssh ... 'sudo iptables -A OUTPUT -d data-api.polymarket.com -j DROP'
# wait 10 minutes
ssh ... 'sudo iptables -D OUTPUT -d data-api.polymarket.com -j DROP'
```

Expected journal output: `fidelity_truth_stale` warnings within 10
minutes; verdict drops to `Warn` or `Fail` for that window. Verdict
recovers once iptables rule is removed.

### b) WS disconnect

Restart the shadow service (simulates WS reconnect):

```bash
ssh ... 'sudo systemctl restart polymarket-shadow'
```

Expected: queue model state file is restored on warm start, no full
warmup needed; first fidelity_event after restart should be `Ok` if
data continuity holds. If queue model resumes NaN, the persistence
path is broken (file a bug, do not promote).

### c) Quiet market window

Pick a low-volume hour (early UTC weekend morning is typical). Verify
that `fidelity_event` rows still emit on the 60s clock with
`shadow_fill_count=0` and `bonereaper_fill_count=0`, MAPE=0, verdict=Ok.
Empty windows must not look like Fail.

## Promotion criteria

Live bonereaper is promoted only when ALL of:

1. ≥ 48h of continuous shadow running.
2. All five market families have `fail=0` over the rolling 24h window.
3. All five market families have `avg_mape < 0.30` over the rolling 24h window.
4. Queue decay rate has stabilized (last 12h `max - min < 0.5` per family).
5. At least one of the failure-mode tests (a/b/c) was exercised and recovered cleanly.

If any criterion is missing, extend the soak by 24h. Do not promote
under "close enough" pressure - the gate's whole point is to be
honest about what it has and has not validated.

## Post-promotion daily check

Once live is up:

```bash
# Run from local each morning:
scp -i ~/.ssh/whale_pair_dublin_ed25519.pem ubuntu@ec2-3-252-32-40.eu-west-1.compute.amazonaws.com:/var/lib/polymarket/shadow_live_bonereaper/journal.jsonl /tmp/shadow.jsonl
scp -i ~/.ssh/whale_pair_dublin_ed25519.pem ubuntu@ec2-3-252-32-40.eu-west-1.compute.amazonaws.com:/var/lib/polymarket/live_bonereaper/journal.jsonl /tmp/live.jsonl

python3 polymarket-exec/scripts/bonereaper_exec_compare.py --mode fidelity-trend --journal-log /tmp/shadow.jsonl
python3 polymarket-exec/scripts/bonereaper_exec_compare.py --mode queue-decay-trend --journal-log /tmp/shadow.jsonl
```

Action thresholds:

- Any `fail > 0` in last 24h → live process should already be `RiskOff` via the gate. Confirm `journalctl -u polymarket-bonereaper-live -n 100 | grep RiskOff`.
- `avg_mape` rising trend over multiple days → the simulator is drifting from reality; investigate before scaling capital.
- Queue decay rate diverging from per-family stable value → market regime change; recalibrate or pause.
