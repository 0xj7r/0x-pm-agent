import json
from pathlib import Path
from strategies.threshold import ThresholdStrategy


def test_signal_up():
    s = ThresholdStrategy("btc", move_threshold=0.08, max_entry=0.55)
    result = s.check_signal(price_move_pct=0.10, token_price_up=0.50, token_price_down=0.52)
    assert result == "Up"


def test_signal_down():
    s = ThresholdStrategy("btc", move_threshold=0.08, max_entry=0.55)
    result = s.check_signal(price_move_pct=-0.10, token_price_up=0.52, token_price_down=0.50)
    assert result == "Down"


def test_no_signal_below_threshold():
    s = ThresholdStrategy("btc", move_threshold=0.08, max_entry=0.55)
    result = s.check_signal(price_move_pct=0.05, token_price_up=0.50, token_price_down=0.50)
    assert result is None


def test_skip_when_entry_too_expensive():
    s = ThresholdStrategy("btc", move_threshold=0.08, max_entry=0.55)
    result = s.check_signal(price_move_pct=0.10, token_price_up=0.70, token_price_down=0.32)
    assert result == "SKIP"


def test_from_config():
    s = ThresholdStrategy.from_config("btc", {"move_threshold": 0.08, "max_entry": 0.55})
    assert s.coin == "btc"
    assert s.move_threshold == 0.08


def test_from_strategy_results():
    results_path = Path("backtesting/strategy_results.json")
    if not results_path.exists():
        return
    data = json.loads(results_path.read_text())
    for coin, info in data.items():
        params = info["best_strategy"]["params"]
        s = ThresholdStrategy.from_config(coin, {
            "move_threshold": params.get("move", 0.08),
            "max_entry": params.get("max_entry", 0.55),
        })
        assert s.move_threshold > 0
        assert s.max_entry > 0
