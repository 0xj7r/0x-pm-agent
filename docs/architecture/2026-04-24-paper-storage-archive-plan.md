# Paper Storage And Archive Plan

## Goal

Keep paper trading cheap and operationally simple:

- hot runtime state stays local in `SQLite`
- active journals stay bounded via local size-based rotation
- cold journals / explicit backups are shipped to `S3`
- old remote objects can transition to `Glacier Deep Archive`

Do **not** use Postgres/Supabase for raw paper journal storage in v1.

## What Changed

### Hot path

- `whale-pair-exec` can now rotate journals locally with:
  - `WHALE_PAIR_EXEC_JOURNAL_ROTATE_BYTES`
- the runtime SQLite now stores:
  - durable orders
  - compact unlawful signal snapshots

That means the journal is no longer the only calibration source.

### Cold path

- new archive script:
  - `whale-pair-exec/scripts/archive_paper_artifacts.sh`
- new user-service artifacts:
  - `whale-pair-exec/ops/systemd/whale-pair-archive.service`
  - `whale-pair-exec/ops/systemd/whale-pair-archive.timer`

The script archives:

- explicit backup directories like `*.pre_fix_*`
- matching runtime backup directories under `data/runtime/*.pre_fix_*`
- rotated journal segments like `journal.<ts>.jsonl`

It intentionally ignores the active `journal.jsonl`.
If it sees very large active journals and no rotated segments, it warns that
rotation may not actually be enabled in the live launcher environment.

## Recommended Layout

### Local

- keep:
  - `whale-pair-exec/data/runtime/*.sqlite`
  - active `journal.jsonl`
  - recent rotated segments waiting for upload

### S3

- upload rotated segments and explicit backup directories
- lifecycle old objects into `Glacier Deep Archive`

### Optional later

- derived rollups can go to Postgres/Supabase later
- raw append-only journal history should stay in object storage

## Environment

Set in `~/.config/whale-pair-exec/common.env` or `.env`:

```bash
WHALE_PAIR_EXEC_JOURNAL_ROTATE_BYTES=268435456
WHALE_PAIR_ARCHIVE_S3_URI=s3://your-bucket/polymarket-agent
WHALE_PAIR_ARCHIVE_STORAGE_CLASS=DEEP_ARCHIVE
WHALE_PAIR_ARCHIVE_MIN_AGE_MINUTES=30
WHALE_PAIR_ARCHIVE_DELETE_LOCAL_AFTER_UPLOAD=false
WHALE_PAIR_ARCHIVE_INCLUDE_PRE_FIX=true
WHALE_PAIR_ARCHIVE_INCLUDE_RUNTIME_PRE_FIX=true
WHALE_PAIR_ARCHIVE_INCLUDE_ROTATED_SEGMENTS=true
WHALE_PAIR_ARCHIVE_ACTIVE_JOURNAL_WARN_BYTES=1073741824
WHALE_PAIR_ARCHIVE_LIST_ONLY=false
WHALE_PAIR_ARCHIVE_DRY_RUN=false
```

Recommended defaults:

- journal rotation: `256 MiB`
- storage class: `DEEP_ARCHIVE`
- min archive age: `30m`
- active journal warning threshold: `1 GiB`
- local delete after upload:
  - `false` first
  - switch to `true` once S3 path is verified

## Example Commands

Dry-run archive pass:

```bash
WHALE_PAIR_ARCHIVE_DRY_RUN=true \
WHALE_PAIR_ARCHIVE_S3_URI=s3://your-bucket/polymarket-agent \
whale-pair-exec/scripts/archive_paper_artifacts.sh
```

Cheap local candidate listing with no AWS dependency:

```bash
WHALE_PAIR_ARCHIVE_LIST_ONLY=true \
whale-pair-exec/scripts/archive_paper_artifacts.sh
```

Manual archive pass:

```bash
WHALE_PAIR_ARCHIVE_S3_URI=s3://your-bucket/polymarket-agent \
whale-pair-exec/scripts/archive_paper_artifacts.sh
```

Enable periodic archive:

```bash
whale-pair-exec/ops/systemd/install_user_paper_services.sh
systemctl --user daemon-reload
systemctl --user enable --now whale-pair-archive.timer
```

## Immediate Cleanup Order

If local disk is critically full:

1. Upload the explicit backup directories:
   - `unlawful-press.pre_fix_20260424T0644`
   - `unlawful-baseline.pre_fix_20260424T0644`
   - `unlawful-broad-hours.pre_fix_20260424T0644`
   - and the matching `data/runtime/*.pre_fix_*` directories
2. Verify objects exist in S3
3. Then delete local backups
4. Leave active paper journals alone until rotation + archive are confirmed

## Operator Checks

Before trusting the archive path, verify both of these:

1. `WHALE_PAIR_EXEC_JOURNAL_ROTATE_BYTES` is present in the real host env
   that systemd or the launcher is using, not just in `.env.example`.
2. Rotated segments are actually appearing under
   `whale-pair-exec/data/execution/paper/*/journal.<ts>.jsonl`.

If the archive script logs:

- `active journal exceeds threshold without any rotated segments present`

then the immediate fix is to correct the live env/launcher wiring, not to
delete `journal.jsonl`.

## If AWS Credentials Are Invalid

Use this order:

1. Run a cheap local listing first:

```bash
WHALE_PAIR_ARCHIVE_LIST_ONLY=true \
whale-pair-exec/scripts/archive_paper_artifacts.sh
```

2. Fix AWS auth until this works:

```bash
aws sts get-caller-identity
```

3. Then do a non-destructive upload test:

```bash
WHALE_PAIR_ARCHIVE_DRY_RUN=true \
WHALE_PAIR_ARCHIVE_S3_URI=s3://your-bucket/polymarket-agent \
whale-pair-exec/scripts/archive_paper_artifacts.sh
```

4. Only after that, run the real archive pass.

## Why This Is Cheap

- `SQLite` is free and already part of the runtime
- `S3` + `Glacier Deep Archive` is the right place for large cold JSONL history
- no always-on DB needed for raw logs
- dashboarding can use compact runtime tables and curated rollups later
