# Whale-Pair Live Bot: Dublin Deployment Spec

Owner: whale-pair strategy team
Target service: `scripts/whale_pair_live_bot.py`
Intended region: Dublin, Ireland (closest allowed region to Polymarket CLOB in AWS `eu-west-2` London)
Status: pre-deployment spec. The live runner script is produced by a parallel workstream and is treated here as an opaque dependency with a known CLI and env surface.

## 1. Why Dublin, and why separate from Hetzner

The existing directional bots (`btc-live-t10`, `btc-sniper`, `eth-sniper`, etc.) run on Hetzner CX-class servers in Falkenstein, Germany (server ID `127613277`, IP `204.168.224.162`) via the repo root `docker-compose.yml`. That stack works for 5-minute skew/threshold strategies where a Falkenstein-to-London RTT in the 18-25 ms range is fine.

The whale-pair strategy is different:

1. It pairs YES+NO legs on the active BTC 5-minute window (and its immediate successor) and only captures edge when it can act on a top-of-book quote before other participants. Per the module docstring in `scripts/whale_pair_live_bot.py`, every submit records `book_ts`, `book_age_ms`, `decision_ts`, `submit_ts` precisely because latency is the dominant risk.
2. Polymarket CLOB matching runs in AWS `eu-west-2` (London). Dublin (AWS `eu-west-1`, Equinix DB1/DB3, or comparable carriers) lands at 8-15 ms RTT to London. Falkenstein lands at 18-25 ms. On a 5-minute window with paired legs, that delta is the difference between filling the ask and chasing a moved book.
3. Hetzner does not offer a Dublin region. Their closest EU options are Falkenstein, Nuremberg, and Helsinki. None are acceptable for this workload.

We therefore run whale-pair on a dedicated Dublin host, isolated from Hetzner:

- Separate host, separate SSH key, separate `.env` file, separate Docker daemon.
- Separate Polymarket funder/proxy and separate `POLYMARKET_PRIVATE_KEY` (do not reuse the Hetzner live key). This means a blast radius of one strategy per wallet: if the whale-pair key is compromised or the bot misbehaves, the Hetzner live capital is not exposed.
- Separate monitoring: logs, alerts, and pager routing for this host carry a `whale-pair` tag so an on-call does not conflate it with the Hetzner bots.
- No shared Docker Compose file with Hetzner. The whale-pair bot lives in `docker-compose.whale-pair.yml` on the Dublin host only. The root `docker-compose.yml` stays Hetzner-only and is not changed by this spec.

The two fleets never share CI state, secrets, or compose topology. If we later need to migrate back to Hetzner for cost, we migrate the Dublin service as a whole; we do not fold it into the Falkenstein stack.

## 2. Provider choice

Recommended, in order of preference:

| Option | Why pick it | Why skip it |
|---|---|---|
| **AWS `eu-west-1` (Dublin)** | Same physical metro as target AWS region. 8-12 ms to `eu-west-2`. Predictable networking. Easy to harden via Security Groups. | Highest per-hour cost. Requires an AWS account we are not otherwise using for trading. |
| **Equinix Metal DB1/DB3** | Bare metal, dedicated networking, same carrier-hotel as major AWS transit. 8-15 ms to London. | Higher minimum commitment, slower provisioning. |
| **DigitalOcean FRA (Frankfurt) fallback** | Fast to provision, cheap, acceptable if Dublin is unavailable in a window. ~15-20 ms to London. | Not Dublin. Use only if AWS/Equinix cannot be provisioned within the deployment window and latency tests still meet the budget. |

Default choice for first deployment: **AWS `eu-west-1`, single `c7i.large` (or `m7i.large`) instance, Ubuntu 24.04 LTS, in a single AZ**.

Networking:
- Public IP with inbound restricted to the operator's static IP for SSH (port 22).
- No inbound HTTP/HTTPS exposure. The bot is outbound-only (to Polymarket CLOB/WSS and to Supabase/Anthropic if enabled).
- Outbound unrestricted within standard egress.

## 3. Host sizing

Minimum viable:
- 2 vCPU, 4 GB RAM, 40 GB SSD.
- Ubuntu 24.04 LTS.
- Docker 27+ with the Compose plugin.
- System clock synced via chrony or systemd-timesyncd. Clock drift corrupts the `book_ts` / `decision_ts` / `submit_ts` telemetry that this strategy relies on for post-mortems.

Recommended:
- 4 vCPU, 8 GB RAM to leave headroom for the WebSocket client, HTTP fallback polling, SQLite ledger writes, and any future observability sidecar.

## 4. Code layout on the host

```
/opt/polymarket-agent/                (git checkout of this repo, deploy branch)
/opt/polymarket-agent/.env            (Dublin-only secrets; 0600)
/opt/polymarket-agent/docker-compose.whale-pair.yml   (generated; see script)
/opt/polymarket-agent/data/           (SQLite ledgers, bind-mounted into the container)
/opt/polymarket-agent/logs/           (optional; stdout is captured by docker logs)
```

The whale-pair bot writes its ledger to `data/whale_pair_live.db` by default (`--db` flag). Back this directory up or snapshot it at least daily (EBS snapshot on AWS, or `rsync` over SSH to a second host). The ledger is the source of truth for open paired positions.

## 5. Env / config checklist

Required env vars (in `/opt/polymarket-agent/.env`, mode `0600`, owner `deploy`):

| Name | Value | Notes |
|---|---|---|
| `POLYMARKET_PRIVATE_KEY` | hex key for the Dublin-only whale-pair wallet | **Not the Hetzner live key.** Generate or rotate a new key for this deployment. |
| `POLYMARKET_SIGNATURE_TYPE` | `1` | POLY_PROXY signing, matching `docker-compose.yml` line 171. |
| `POLYMARKET_FUNDER` | `0x...` proxy wallet address | Must correspond to the private key above. |
| `STRATEGY_CONFIG` | `/app/strategy_config.json` | Inherited from repo; no whale-pair profile is required because the live runner carries its own CLI flags. |
| `ALLOW_LIVE_WITHOUT_SUPABASE` | `1` during initial deployment | Omit once Supabase credentials are added so trade persistence is enforced. |
| `LOG_LEVEL` | `INFO` | Use `DEBUG` only during incident response. |

Optional env vars (enable once the baseline is green):

| Name | Purpose |
|---|---|
| `SUPABASE_URL`, `SUPABASE_KEY` | Trade persistence; remove `ALLOW_LIVE_WITHOUT_SUPABASE` once set. |
| `ANTHROPIC_API_KEY` | Only if the researcher sidecar is added later. Not required for the live bot. |

Whale-pair CLI flags exposed by `scripts/whale_pair_live_bot.py`:

| Flag | Default | Notes |
|---|---|---|
| `--db` | `data/whale_pair_live.db` | SQLite ledger path inside the container. |
| `--loop` | `15` | Poll cadence in seconds. |
| `--execute` | off | **Omit for dry-run.** Only include after a full dry-run shift passes health checks. |
| `--max-pair-cost` | `0.99` | Risk guardrail: combined YES+NO cost ceiling. |
| `--base-clip-usd` | `50.0` | Baseline per-leg size. Start at a lower number (e.g. `10.0`) for the first 24 h live. |
| `--aggressive-clip-usd` | `250.0` | Upper band. Keep at default only after baseline shift is clean. |
| `--max-gross-cost-usd` | `1000.0` | Aggregate exposure cap. Reduce for the first shift. |
| `--min-seconds-from-start` | `0` | Entry gating window lower bound. |
| `--max-seconds-from-start` | `298` | Entry gating window upper bound. |
| `--completion-min-pnl-per-share` | `0.002` | Merge/completion gating. |
| `--no-ws` | off | Disable WebSocket, force HTTP polling. Use only for emergency fallback; defeats the point of Dublin. |
| `--ws-max-age-ms` | `2000.0` | Book freshness threshold before HTTP fallback kicks in. |

What is still unknown and must be supplied by the operator before deployment:

1. **Dublin wallet private key**. Generate a fresh key; do not reuse the Falkenstein key.
2. **Funder proxy address**. Corresponds to the new key. Must be funded with USDC.e on Polygon before live flag is set.
3. **Provider choice confirmation**. Default is AWS `eu-west-1`; if finance prefers Equinix or another vendor, the host provisioning step changes but nothing else in this spec does.
4. **Latency budget confirmation**. Measured RTT from the provisioned host to a Polymarket CLOB REST endpoint should be under 15 ms p50. If over, rethink provider before unlocking `--execute`.
5. **Starting capital and per-shift caps**. The defaults above (`--base-clip-usd=50`, `--max-gross-cost-usd=1000`) are from the script. Operator must confirm starting caps for the first live shift and override via flags.
6. **Monitoring/alert destination**. Where do `docker logs` stream? Where do health-check failures page? Default in this spec is stdout + manual inspection; wire to Loki/Grafana or similar before the bot is left unattended.
7. **Backup cadence for `data/whale_pair_live.db`**. Default proposal: nightly `rsync` to a second host. Operator must confirm.
8. **Failover operator path**. Promotion should run through `promote_standby.sh`, not raw `start.sh --live`, so split-brain guardrails stay intact.

## 6. Runbook

### 6.1 Initial provisioning

```
# On operator workstation
./scripts/deploy/whale-pair/provision.sh <host-ip>
```

This script (see `scripts/deploy/whale-pair/provision.sh`):
1. SSHes to the host.
2. Installs Docker + Compose plugin, git, chrony.
3. Creates `/opt/polymarket-agent`, clones the repo at the `feat/whale-pair-deploy` branch (or `main` once merged).
4. Creates `data/` with the correct ownership.
5. Prompts for the `.env` contents and writes them with mode `0600`.
6. Validates that `.env` contains all required vars using `scripts/deploy/whale-pair/check_env.sh`.
7. Does **not** start the bot. Starting is a separate, explicit step.

### 6.2 Dry-run start

A dry-run is the bot running against live Polymarket market data but without `--execute`, so no orders are submitted.

```
ssh deploy@<host>
cd /opt/polymarket-agent
./scripts/deploy/whale-pair/start.sh --dry-run
```

`start.sh --dry-run` builds the image, starts the `whale-pair-live` service from `docker-compose.whale-pair.yml` without the `--execute` flag, and tails the container logs for the first 60 seconds so the operator can see the handshake to Polymarket, the first WS subscription, and the first `book_ts` / `book_age_ms` emission.

Expected first-60-seconds signals:
- Log line containing `Polymarket` client init and a non-zero `POLYMARKET_FUNDER`.
- Log line containing `ws` or `subscribe` indicating the WebSocket is attached to the active BTC outcome tokens.
- Periodic telemetry with `source=ws` and `book_age_ms` under `--ws-max-age-ms` (default 2000).

Red flags that block promotion to `--execute`:
- Any log containing `ALLOW_LIVE_WITHOUT_SUPABASE` where the operator intended Supabase to be wired.
- `book_age_ms` persistently above 2000 ms (WS is stale, HTTP fallback will carry the load, Dublin advantage is gone).
- Any unhandled exception in the first loop iteration.

### 6.3 Live start (after dry-run passes)

```
./scripts/deploy/whale-pair/start.sh --live
```

Adds `--execute` to the bot command. Starts with reduced size flags for the first shift (edit `docker-compose.whale-pair.yml` or pass through the start script's `--base-clip-usd` arg, which defaults to a conservative `10.0` for the first live shift).

On a passive standby host, direct `start.sh --live` is intentionally blocked. Use `promote_standby.sh --confirm-primary-stopped` after standby status and health checks pass.

### 6.4 Health checks

Run these at least every 15 minutes during the first 24 h live, then at least hourly:

```
./scripts/deploy/whale-pair/health.sh
```

`health.sh` verifies:
1. Container is up (`docker compose ps` shows `running`).
2. Container has emitted a log line in the last 60 s (`docker compose logs --since 60s | wc -l > 0`).
3. Recent log lines include a `book_ts` (bot is ingesting book data, not stuck in a reconnect loop).
4. Last recorded `book_age_ms` is under `ws_max_age_ms` (freshness).
5. SQLite ledger file exists and was modified in the last 5 minutes (bot is actively writing).
6. Disk free on `/opt/polymarket-agent/data` is above 5 GB.
7. System clock drift under 100 ms (via `chronyc tracking`).

If any check fails, the script exits non-zero and prints which check failed. Wire it to a monitoring system or run from cron with alerting on non-zero exit.

### 6.5 Logs to inspect

Primary:
```
docker compose -f docker-compose.whale-pair.yml logs -f whale-pair-live
```

Secondary (per-incident):
- `docker compose -f docker-compose.whale-pair.yml logs --since 1h whale-pair-live | grep -E 'book_age_ms|decision_ts|submit_ts'`
  Latency telemetry audit.
- `docker compose -f docker-compose.whale-pair.yml logs --since 1h whale-pair-live | grep -iE 'error|traceback|exception'`
  Unhandled error audit.
- `sqlite3 data/whale_pair_live.db '.tables'` then inspect the ledger tables for open paired positions.

What to watch for:
- `book_age_ms` trending upward across the shift. Either the WS is degrading or the host is under network pressure.
- Repeated `source=http` fallback. Indicates the WS is failing; check the Polymarket WS client logs.
- Any decision with `submit_ts - decision_ts > 200 ms`. The bot is CPU-bound or blocked on I/O. Host is undersized or contended.

### 6.6 Kill switch

Primary (soft): remove the `--execute` flag and restart. The bot continues tracking markets and writing telemetry but stops submitting orders. Open paired positions are **not** touched; they remain in the ledger and must be resolved manually or by re-enabling `--execute` after the issue is understood.

```
./scripts/deploy/whale-pair/kill.sh --soft
```

Primary (hard): stop the container immediately.

```
./scripts/deploy/whale-pair/kill.sh --hard
```

`kill.sh --hard` runs `docker compose -f docker-compose.whale-pair.yml stop whale-pair-live`. This leaves open paired positions on Polymarket untouched. After a hard kill, the operator MUST:
1. Check the ledger for open inventory (`sqlite3 data/whale_pair_live.db 'select market_id, side, shares_remaining from whale_pair_open_lots where shares_remaining > 0;'`; see `core/whale_pair_ledger.py`).
2. Decide whether to resolve them manually via the Polymarket UI, let them run to settlement, or restart the bot in execute mode to let it complete/merge them.

Nuclear: revoke the Polymarket API key for this wallet (on the Polymarket side) and/or move funds out of the funder proxy. Only use if the bot is suspected of misbehaving beyond what a container stop can contain.

### 6.7 Rollback

"Rollback" for this service means one of:

1. **Revert the deploy branch.** On the host:
   ```
   cd /opt/polymarket-agent
   ./scripts/deploy/whale-pair/kill.sh --hard
   git fetch origin
   git checkout <previous-good-sha>
   ./scripts/deploy/whale-pair/start.sh --dry-run
   ```
   Promote back to `--live` only after dry-run passes.

2. **Disable the service entirely.** Same as kill `--hard`, plus disable the systemd unit or crontab entry that restarts it.

3. **Point back to Hetzner.** This is a no-op. The Hetzner stack is unchanged and continues running its own bots. The whale-pair strategy is simply offline until a new Dublin host is ready.

Do **not** attempt to run the whale-pair bot on the Hetzner host as a rollback. The latency budget does not permit it.

## 7. What this spec intentionally does not do

- No changes to `strategies/whale_pair.py`, `backtesting/whale_pair_backtest.py`, `core/whale_pair_ledger.py`, or the root `docker-compose.yml`.
- No promotion of the whale-pair service into the Hetzner Compose file.
- No automated CI deployment. First deployment is manual and operator-driven; automation is a follow-up once the shape is proven.
- No shared secrets with the Hetzner stack. The Dublin `.env` is created by hand on the Dublin host.

## 8. Files introduced by this spec

- `docs/deploy/whale-pair-dublin.md` (this document)
- `scripts/deploy/whale-pair/provision.sh`
- `scripts/deploy/whale-pair/check_env.sh`
- `scripts/deploy/whale-pair/start.sh`
- `scripts/deploy/whale-pair/health.sh`
- `scripts/deploy/whale-pair/kill.sh`
- `scripts/deploy/whale-pair/replicate_backup.sh`
- `scripts/deploy/whale-pair/install_backup_replication_cron.sh`
- `scripts/deploy/whale-pair/promote_standby.sh`
- `scripts/deploy/whale-pair/docker-compose.whale-pair.yml` (template, copied to host during provisioning)
- `tests/test_whale_pair_deploy_scripts.py` (script-level sanity tests)
