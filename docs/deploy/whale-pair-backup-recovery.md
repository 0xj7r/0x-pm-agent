# Whale-Pair Backup and Recovery Runbook

Owner: whale-pair strategy team  
Scope: us-east primary + us-east standby  
Data root: `/opt/polymarket-agent/data`

## Purpose

Make the whale-pair deploy operationally recoverable without changing runtime
code. The ledger and local deploy artifacts live under `/opt/polymarket-agent/data`,
so that directory is the recovery boundary.

What is backed up:

- `whale_pair_live.db`
- any other SQLite ledgers under `data/`
- local state files and role markers

What is not backed up:

- Docker images
- `.env`
- the git checkout

Those are rebuilt from the repo and operator secrets. Recovery is therefore:

1. restore repo checkout
2. restore `.env`
3. restore `/opt/polymarket-agent/data`
4. start dry-run
5. verify health
6. promote only if safe

## Backup Artifact

Script: [scripts/deploy/whale-pair/backup_data.sh](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/backup_data.sh)
Replication helper: [scripts/deploy/whale-pair/replicate_backup.sh](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/replicate_backup.sh)  
Cron installer: [scripts/deploy/whale-pair/install_backup_replication_cron.sh](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/install_backup_replication_cron.sh)

Default output:

- archive: `/opt/polymarket-agent/data/backups/whale-pair-data-<host>-<utc>.tar.gz`
- checksum: same basename + `.sha256`
- manifest: same basename + `.manifest.json`

Behavior:

- snapshots SQLite files with `sqlite3 .backup` when available
- excludes `data/backups/` from the archive payload
- keeps the newest 7 archives by default
- can invoke a replication hook immediately after the local backup succeeds

## Primary Backup Commands

Run on the us-east primary host:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/backup_data.sh
```

Named backup before maintenance:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/backup_data.sh --name pre-maintenance --keep 14
```

Recommended cron on the primary:

```cron
17 0 * * * cd /opt/polymarket-agent && bash scripts/deploy/whale-pair/backup_data.sh --keep 14 >> logs/whale-pair-backup.log 2>&1
```

## Off-Host Copy

The local backup is not enough on its own. Copy it to the standby or to
object storage.

Example push to the standby:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/replicate_backup.sh --latest --dest deploy@us-east-standby:/opt/polymarket-agent/data/backups/
```

To couple backup creation with immediate replication:

```bash
cd /opt/polymarket-agent
BACKUP_REPLICA_DEST=deploy@us-east-standby:/opt/polymarket-agent/data/backups/ \
  bash scripts/deploy/whale-pair/backup_data.sh \
    --keep 14 \
    --replicate-hook scripts/deploy/whale-pair/replicate_backup.sh
```

To install a recurring nightly backup + replication cron:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/install_backup_replication_cron.sh \
  --dest deploy@us-east-standby:/opt/polymarket-agent/data/backups/
```

## Restore Script

Script: [scripts/deploy/whale-pair/restore_data.sh](/Users/jackreid/go/polymarket-agent/scripts/deploy/whale-pair/restore_data.sh)

Restore a specific archive:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/restore_data.sh /opt/polymarket-agent/data/backups/whale-pair-data-<host>-<utc>.tar.gz --target /opt/polymarket-agent/data --force
```

Restore the latest available archive:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/restore_data.sh --latest --backup-dir /opt/polymarket-agent/data/backups --target /opt/polymarket-agent/data --force
```

Behavior:

- refuses to overwrite a non-empty target unless `--force` is supplied
- moves the existing target aside to `data.pre-restore.<utc>`
- restores the archived `data/` tree into the target dir
- verifies the sibling `.sha256` file when present
- writes `whale_pair_restore.meta` into the restored target for standby status and failover guardrails

## Recovery Procedure: Fresh Host

1. Provision host and checkout repo.
2. Restore `.env` manually with mode `0600`.
3. Copy a backup archive plus checksum/manifest to the host.
4. Restore data:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/restore_data.sh /opt/polymarket-agent/data/backups/whale-pair-data-<host>-<utc>.tar.gz --target /opt/polymarket-agent/data --force
```

5. Validate env:

```bash
bash scripts/deploy/whale-pair/check_env.sh /opt/polymarket-agent/.env
```

6. Start dry-run:

```bash
bash scripts/deploy/whale-pair/start.sh --dry-run
```

7. Run health:

```bash
bash scripts/deploy/whale-pair/health.sh
```

8. Inspect ledger:

```bash
sqlite3 /opt/polymarket-agent/data/whale_pair_live.db '.tables'
sqlite3 /opt/polymarket-agent/data/whale_pair_live.db "SELECT COUNT(*) FROM whale_pair_actions;"
```

9. Promote to live only after manual review.

## Recovery Procedure: Primary Loss, Standby Promotion

If the primary is down and the standby has the latest restored data:

1. Confirm standby ledger state:

```bash
cd /opt/polymarket-agent
bash scripts/deploy/whale-pair/standby_status.sh
```

2. Confirm dry-run health:

```bash
bash scripts/deploy/whale-pair/health.sh
```

3. Stop the failed primary if it is still reachable enough to be dangerous.
4. Promote standby manually:

```bash
bash scripts/deploy/whale-pair/promote_standby.sh --confirm-primary-stopped
```

5. Record the promotion time in the incident log and take a fresh backup after the first stable cycle.

## Validation Drill

At least once before funding meaningful size:

1. create manual backup
2. restore it into a scratch host or a scratch data dir
3. start dry-run
4. confirm the ledger opens cleanly and the bot stays healthy

This runbook is not done until that drill has been performed successfully.
