"""Entry point for the BTC 5-minute sniper agent.

Usage:
    python btc_main.py                    # paper trading (default)
    python btc_main.py --live             # live trading
    python btc_main.py --config path.json # custom config
"""
from __future__ import annotations

import argparse
import asyncio
import logging
import os
import signal
import sys
from pathlib import Path

from dotenv import load_dotenv

load_dotenv()


def setup_logging(level: str = "INFO") -> None:
    logging.basicConfig(
        level=getattr(logging, level.upper(), logging.INFO),
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    )


def write_pid() -> None:
    pid_file = Path("btc_agent.pid")
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


def cleanup_pid() -> None:
    pid_file = Path("btc_agent.pid")
    if pid_file.exists():
        pid_file.unlink()


async def main(config_path: str, live: bool, coin: str) -> None:
    from core.engine import BTCTradingEngine
    from strategies.live_runtime import LiveRuntimeStrategy
    from strategies.strategy_config import CoinStrategyConfig, load_strategy_config

    cfg = load_strategy_config(config_path)
    if live:
        cfg.paper.enabled = False

    db_path = os.getenv("BTC_DB_PATH", f"{coin}_trades.db")
    engine = BTCTradingEngine(cfg, db_path=db_path, coin=coin)

    loop = asyncio.get_event_loop()

    def handle_shutdown(sig: int, frame: object) -> None:
        logging.info(f"Received signal {sig}, shutting down...")
        loop.create_task(engine.stop())

    signal.signal(signal.SIGINT, handle_shutdown)
    signal.signal(signal.SIGTERM, handle_shutdown)

    def handle_reload(sig: int, frame: object) -> None:
        logging.info("SIGHUP received, reloading strategy config...")
        new_cfg = load_strategy_config(config_path)
        engine.cfg = new_cfg
        from dataclasses import asdict
        coin_conf = new_cfg.coins.get(coin, CoinStrategyConfig())
        engine._strategy = LiveRuntimeStrategy.from_config(coin, asdict(coin_conf))
        logging.info("Config reloaded")

    signal.signal(signal.SIGHUP, handle_reload)

    async def heartbeat() -> None:
        while engine._running:
            Path("btc_heartbeat.txt").write_text(
                f"{__import__('datetime').datetime.now().isoformat()}"
            )
            await asyncio.sleep(30)

    asyncio.create_task(heartbeat())

    try:
        await engine.run()
    finally:
        await engine.stop()
        cleanup_pid()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Polymarket 5-Minute Sniper Agent")
    parser.add_argument("--config", default="strategy_config.json", help="Path to strategy config JSON")
    parser.add_argument("--live", action="store_true", help="Enable live trading (default: paper)")
    parser.add_argument("--coin", default="btc", choices=["btc", "eth", "sol"], help="Coin to trade (default: btc)")
    parser.add_argument("--log-level", default="INFO", help="Log level")
    args = parser.parse_args()

    setup_logging(args.log_level)
    write_pid()

    try:
        asyncio.run(main(args.config, args.live, args.coin))
    finally:
        cleanup_pid()
