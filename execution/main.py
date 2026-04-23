"""Entry point for the Polymarket sniper agent.

Usage:
    python main.py                              # paper trading, default profile
    python main.py --profile relaxed            # alternate config profile
    python main.py --live                       # live trading
    python main.py --config path.json           # custom config
"""
from __future__ import annotations

import argparse
import asyncio
import hashlib
import logging
import os
import signal
import sys
from pathlib import Path

from dotenv import load_dotenv

load_dotenv()

_LOCK_FH = None


def setup_logging(level: str = "INFO") -> None:
    logging.basicConfig(
        level=getattr(logging, level.upper(), logging.INFO),
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    )


def _instance_scope_name(
    *,
    live: bool,
    coin: str,
    market_type: str,
    profile: str | None,
) -> str:
    parts = [coin.lower(), market_type, "live" if live else "paper"]
    if profile:
        parts.append(profile)
    if live:
        private_key = os.getenv("POLYMARKET_PRIVATE_KEY", "")
        if private_key:
            wallet_hash = hashlib.sha256(private_key.encode("utf-8")).hexdigest()[:12]
            parts.append(wallet_hash)
    return "-".join(parts)


def write_pid(scope_name: str = "btc_agent") -> None:
    global _LOCK_FH
    pid_file = Path(f"{scope_name}.pid")
    lock_file = Path(f"{scope_name}.lock")

    # Advisory file lock: survives stale PID files and blocks duplicate
    # containers that share a mounted data/app volume.
    import fcntl
    lock_fh = open(lock_file, "a+")
    try:
        fcntl.flock(lock_fh.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        print(f"Agent lock already held ({lock_file}). Exiting.")
        sys.exit(1)
    _LOCK_FH = lock_fh

    if pid_file.exists():
        try:
            old_pid = int(pid_file.read_text().strip())
            if old_pid == os.getpid():
                pid_file.unlink()
            else:
                os.kill(old_pid, 0)
                print(f"Agent already running (PID {old_pid}). Exiting.")
                sys.exit(1)
        except (OSError, ValueError):
            pass
    pid_file.write_text(str(os.getpid()))


def cleanup_pid(scope_name: str = "btc_agent") -> None:
    global _LOCK_FH
    pid_file = Path(f"{scope_name}.pid")
    if pid_file.exists():
        pid_file.unlink()
    if _LOCK_FH is not None:
        try:
            import fcntl
            fcntl.flock(_LOCK_FH.fileno(), fcntl.LOCK_UN)
        except OSError:
            pass
        _LOCK_FH.close()
        _LOCK_FH = None


async def main(config_path: str, live: bool, coin: str, market_type: str) -> None:
    from core.engine import BTCTradingEngine
    from strategies.live_runtime import LiveRuntimeStrategy
    from strategies.strategy_config import CoinStrategyConfig, load_strategy_config

    profile = os.getenv("STRATEGY_PROFILE")
    cfg = load_strategy_config(config_path, profile=profile)
    if live:
        cfg.paper.enabled = False

    db_path = os.getenv("BTC_DB_PATH", f"{coin}_trades.db")
    engine = BTCTradingEngine(cfg, db_path=db_path, coin=coin, market_type=market_type)

    loop = asyncio.get_event_loop()

    def handle_shutdown(sig: int, frame: object) -> None:
        logging.info(f"Received signal {sig}, shutting down...")
        loop.create_task(engine.stop())

    signal.signal(signal.SIGINT, handle_shutdown)
    signal.signal(signal.SIGTERM, handle_shutdown)

    def handle_reload(sig: int, frame: object) -> None:
        logging.info("SIGHUP received, reloading strategy config...")
        new_cfg = load_strategy_config(config_path, profile=profile)
        engine.cfg = new_cfg
        coin_conf = new_cfg.coins.get(coin, CoinStrategyConfig())
        engine.set_strategy(LiveRuntimeStrategy.from_config(coin, {
            "strategy": coin_conf.strategy,
            "params": dict(coin_conf.params),
        }))
        logging.info("Config reloaded")

    signal.signal(signal.SIGHUP, handle_reload)

    scope_name = _instance_scope_name(
        live=live,
        coin=coin,
        market_type=market_type,
        profile=profile,
    )
    heartbeat_file = Path(f"{scope_name}.heartbeat.txt")

    async def heartbeat() -> None:
        while engine._running:
            heartbeat_file.write_text(
                f"{__import__('datetime').datetime.now().isoformat()}"
            )
            await asyncio.sleep(30)

    asyncio.create_task(heartbeat())

    try:
        await engine.run()
    finally:
        await engine.stop()
        cleanup_pid(scope_name)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Polymarket 5-Minute Sniper Agent")
    parser.add_argument("--config", default=os.getenv("STRATEGY_CONFIG", "strategy_config.json"), help="Path to strategy config JSON")
    parser.add_argument("--profile", dest="profile", default=os.getenv("STRATEGY_PROFILE"), help="Named config profile from the canonical strategy config")
    parser.add_argument("--live", action="store_true", help="Enable live trading (default: paper)")
    parser.add_argument("--coin", default="btc", choices=["btc", "eth", "sol", "doge"], help="Coin to trade (default: btc)")
    parser.add_argument("--market-type", default="5m", choices=["5m", "15m"], help="Market window length (default: 5m)")
    parser.add_argument("--log-level", default="INFO", help="Log level")
    args = parser.parse_args()

    if args.profile:
        os.environ["STRATEGY_PROFILE"] = args.profile

    setup_logging(args.log_level)
    scope_name = _instance_scope_name(
        live=args.live,
        coin=args.coin,
        market_type=args.market_type,
        profile=args.profile,
    )
    write_pid(scope_name)

    try:
        asyncio.run(main(args.config, args.live, args.coin, args.market_type))
    finally:
        cleanup_pid(scope_name)
