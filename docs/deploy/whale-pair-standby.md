# Whale-Pair Passive Standby Runbook

Owner: whale-pair strategy team  
Primary region: `us-east-1`  
Standby role: passive dry-run mirror with restored ledger state

## Goal

Keep a us-east standby host warm enough to take over with one operator action,
without turning it into an active-active trading setup.

Passive standby means:

- same repo checkout
- same deploy scripts
- same `.env` shape
- same market subscriptions
- dry-run mode by default
- restored copy of the latest `/opt/polymarket-agent/data` backup
- no `--execute` unless an operator promotes it

## Standby Scripts

- bootstrap: [scripts/deploy/whale-pair/standby_bootstrap.sh](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/standby_bootstrap.sh)
- status: [scripts/deploy/whale-pair/standby_status.sh](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/standby_status.sh)
- restore helper: [scripts/deploy/whale-pair/restore_data.sh](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/restore_data.sh)

## Bootstrap the Standby

On the standby host, after repo checkout and `.env` placement:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/standby_bootstrap.sh --restore-latest --primary us-east-primary
```

What it does:

1. validates `.env` with `check_env.sh`
2. restores the latest backup from `data/backups/` if requested
3. writes `data/whale_pair_standby.role`
4. starts the service in dry-run mode with `start.sh --dry-run`

If you want to restore but not start yet:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/standby_bootstrap.sh --restore-latest --primary us-east-primary --no-start
```

If you already validated `.env` separately:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/standby_bootstrap.sh --restore-latest --skip-env-check
```

## Standby Status

Run:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/standby_status.sh
```

It shows:

- standby role marker contents
- ledger path, size, and modification time
- latest backup archive present locally
- current Docker Compose service status

## Daily Operator Loop

1. Primary creates backup:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/backup_data.sh
```

2. Copy latest archive to standby.
3. On standby, refresh from that archive:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/standby_bootstrap.sh --restore-latest --primary us-east-primary
```

4. Verify standby:

```bash
bash scripts/deploy/whale-pair/standby_status.sh
bash scripts/deploy/whale-pair/health.sh
```

## Promotion Procedure

Promotion is manual on purpose.

Before promoting the standby:

1. stop or isolate the primary if there is any chance it could still submit orders
2. confirm the standby has the latest known-good ledger restore
3. confirm the standby dry-run service is healthy

Promotion command:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/start.sh --live
```

After promotion:

1. monitor logs continuously for the first session
2. run `health.sh` more frequently than normal
3. create a new backup from the promoted host once the session stabilizes

## Failback Procedure

When the original primary is repaired:

1. treat it as a fresh standby candidate, not as an authoritative node
2. copy the newest backup from the active host
3. restore it onto the repaired host
4. start dry-run only
5. verify health and status

Do not reintroduce it as live until there is a deliberate handoff.

## Constraints

- This is not automated replication.
- This does not guarantee zero data loss between the last copied backup and the failure moment.
- `.env` and wallet custody remain manual operator responsibilities.

That is acceptable for the current shadow/live stage, but it is the next obvious place to automate once the strategy is stable enough to justify it.
