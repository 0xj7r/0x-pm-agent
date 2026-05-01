# BANDAIDS.md

This file tracks engine anti-patterns and environment-name coverage so runtime
behavior does not drift into shell wrappers or silent env mismatches.

## Anti-patterns to avoid

| Anti-pattern | Correct fix |
|---|---|
| Bash loop that restarts the bot to refresh markets | Add market-context refresh inside the Rust runtime. |
| Shell-side strategy gates | Add typed gates in the Rust strategy/risk engine. |
| Sleep-based rescue throttles in launch scripts | Track in-flight rescues per market inside engine state. |
| Env-only override for missing engine behavior | Add the missing Rust code path and test the seam. |

## Env coverage table

| Env var | Parsed in | Launcher coverage | Notes |
|---|---|---|---|
| `PM_BTC_5M_DISABLE_SINGLETON_LOCK` | `polymarket-exec/src/main.rs` | Optional operator override | Disables the live singleton lock. Paper mode bypasses the lock automatically. |
| `PM_BTC_5M_EXEC_LOCK_PATH` | `polymarket-exec/src/main.rs` | Optional operator override | Overrides the default live lockfile path `/tmp/polymarket-exec.live.lock`. |
| `POLYGON_RPC_URL` | `polymarket-exec/src/config/mod.rs`, `polymarket-exec/src/wire/polygon_rpc.rs` | Required for EOA merge/redeem/wrap | Primary Polygon RPC endpoint. Transaction sends use only the primary endpoint to avoid duplicate broadcasts. |
| `POLYGON_RPC_FAILOVER_URLS` | `polymarket-exec/src/wire/polygon_rpc.rs` | Optional operator override | Comma-separated failovers used for read/preflight calls such as `eth_call` / `eth_blockNumber`. |
| `POLYGON_RPC_REQUEST_TIMEOUT_MS` | `polymarket-exec/src/wire/polygon_rpc.rs` | Optional operator override | Timeout for JSON-RPC health/preflight requests. |
| `POLYGON_RPC_RECEIPT_TIMEOUT_MS` | `polymarket-exec/src/wire/polygon_rpc.rs` | Optional operator override | Receipt wait timeout for Polygon transaction submissions. |
| `POLYGON_RPC_GAS_LIMIT` | `polymarket-exec/src/wire/polygon_rpc.rs` | Optional operator override | Gas limit for direct EOA Polygon contract calls. |

Before adding another env knob, grep both directions:

```sh
grep -rn "MY_NEW_VAR" polymarket-exec/src polymarket-exec/scripts polymarket-exec/env
```

If a knob is removed from the binary, remove it from launchers/env files. If it
is removed from launchers/env files, remove it from the binary unless it remains
an intentional operator-only escape hatch documented in this table.
