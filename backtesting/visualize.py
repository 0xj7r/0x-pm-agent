"""Generate HTML Monte Carlo fan chart visualization.

Runs stochastic simulation and outputs an interactive HTML file
with P5/P25/median/P75/P95 equity curves.

Usage:
    python backtesting/visualize.py
    python backtesting/visualize.py --output chart.html --sims 3000
"""
from __future__ import annotations

import argparse
import json
import logging
import random
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from backtesting.projection import MonteCarloSimulator, slippage
from shared.fees import taker_fee

logger = logging.getLogger(__name__)
DB_PATH = Path(__file__).parent / "historical.db"


def run_simulation_paths(
    observed_trades,
    win_rate: float,
    starting_balance: float = 100.0,
    bet_pct: float = 0.10,
    liquidity_fill_pct: float = 0.20,
    num_sims: int = 2000,
    months: int = 6,
    trades_per_day: float = 10.0,
    snapshot_interval: int = 10,
) -> dict:
    """Run Monte Carlo and capture equity curve snapshots for charting."""
    entry_prices = [t.entry_price for t in observed_trades]
    liquidities = [t.liquidity for t in observed_trades]
    total_trades = int(trades_per_day * 30 * months)
    num_snapshots = total_trades // snapshot_interval

    all_paths = []

    for sim in range(num_sims):
        rng = random.Random(sim * 7919 + 42)
        balance = starting_balance
        path = [balance]

        for trade_num in range(total_trades):
            entry = rng.choice(entry_prices)
            fee_rate = taker_fee(entry)
            liq = rng.choice(liquidities)

            bet = min(balance * bet_pct, liq * liquidity_fill_pct)
            if bet < 1.0:
                for _ in range(num_snapshots - len(path) + 1):
                    path.append(0)
                break

            slip = slippage(bet, liq)
            effective_entry = entry * (1 + slip)
            if effective_entry >= 0.99:
                if (trade_num + 1) % snapshot_interval == 0:
                    path.append(balance)
                continue

            shares = bet / effective_entry
            fee_usd = bet * fee_rate
            won = rng.random() < win_rate

            if won:
                balance += shares * 1.0 - bet - fee_usd
            else:
                balance -= bet + fee_usd

            if balance <= 0:
                for _ in range(num_snapshots - len(path) + 1):
                    path.append(0)
                break

            if (trade_num + 1) % snapshot_interval == 0:
                path.append(balance)

        all_paths.append(path)

    # Compute percentiles at each snapshot
    max_len = max(len(p) for p in all_paths)
    for p in all_paths:
        while len(p) < max_len:
            p.append(p[-1])

    percentiles = {"p5": [], "p25": [], "p50": [], "p75": [], "p95": []}
    sample_paths_data = [all_paths[i] for i in range(min(20, num_sims))]

    for step in range(max_len):
        vals = sorted(p[step] for p in all_paths)
        n = len(vals)
        percentiles["p5"].append(round(vals[int(n * 0.05)], 2))
        percentiles["p25"].append(round(vals[int(n * 0.25)], 2))
        percentiles["p50"].append(round(vals[n // 2], 2))
        percentiles["p75"].append(round(vals[int(n * 0.75)], 2))
        percentiles["p95"].append(round(vals[int(n * 0.95)], 2))

    days_per_snapshot = (snapshot_interval / trades_per_day)
    x_labels = [round(i * days_per_snapshot, 1) for i in range(max_len)]

    return {
        "x_labels": x_labels,
        "percentiles": percentiles,
        "sample_paths": [p[:max_len] for p in sample_paths_data[:5]],
        "win_rate": win_rate,
        "num_sims": num_sims,
        "months": months,
        "trades_per_day": trades_per_day,
        "starting_balance": starting_balance,
    }


HTML_TEMPLATE = """<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>Monte Carlo Projection</title>
<script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.0/dist/chart.umd.min.js"></script>
<style>
  body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
         background: #0d1117; color: #c9d1d9; margin: 0; padding: 20px; }}
  .container {{ max-width: 1200px; margin: 0 auto; }}
  h1 {{ color: #58a6ff; font-size: 1.4em; }}
  .stats {{ display: grid; grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
            gap: 12px; margin: 16px 0; }}
  .stat {{ background: #161b22; border: 1px solid #30363d; border-radius: 6px;
           padding: 12px; }}
  .stat .label {{ color: #8b949e; font-size: 0.85em; }}
  .stat .value {{ color: #58a6ff; font-size: 1.3em; font-weight: 600; margin-top: 4px; }}
  .chart-container {{ background: #161b22; border: 1px solid #30363d; border-radius: 6px;
                      padding: 16px; margin: 16px 0; }}
  canvas {{ max-height: 500px; }}
  .note {{ color: #8b949e; font-size: 0.8em; margin-top: 8px; }}
</style>
</head>
<body>
<div class="container">
  <h1>BTC 5m Latency Arb: Monte Carlo Projection</h1>

  <div class="stats">
    <div class="stat"><div class="label">Simulations</div><div class="value">{num_sims}</div></div>
    <div class="stat"><div class="label">Win Rate</div><div class="value">{win_rate:.0%}</div></div>
    <div class="stat"><div class="label">Trades/Day</div><div class="value">{trades_per_day:.0f}</div></div>
    <div class="stat"><div class="label">Period</div><div class="value">{months} months</div></div>
    <div class="stat"><div class="label">Starting Balance</div><div class="value">${starting_balance:,.0f}</div></div>
    <div class="stat"><div class="label">Median Final</div><div class="value" id="median-final"></div></div>
  </div>

  <div class="chart-container">
    <canvas id="fanChart"></canvas>
  </div>

  <div class="chart-container">
    <canvas id="logChart"></canvas>
  </div>

  <p class="note">
    Includes: Polymarket dynamic fees (0.072*p*(1-p)), slippage model, liquidity cap (20% of market depth).
    Shaded bands show P5-P95 and P25-P75 confidence intervals. Solid line is median.
  </p>
</div>

<script>
const data = {chart_data_json};

document.getElementById('median-final').textContent =
  '$' + data.percentiles.p50[data.percentiles.p50.length-1].toLocaleString(undefined, {{maximumFractionDigits: 0}});

function makeDatasets(useLog) {{
  const transform = useLog ? v => Math.log10(Math.max(v, 1)) : v => v;
  return [
    {{
      label: 'P5-P95 band',
      data: data.x_labels.map((x, i) => ({{ x, y: transform(data.percentiles.p95[i]) }})),
      fill: '+1',
      backgroundColor: 'rgba(88, 166, 255, 0.08)',
      borderColor: 'rgba(88, 166, 255, 0.2)',
      borderWidth: 1,
      pointRadius: 0,
    }},
    {{
      label: 'P5',
      data: data.x_labels.map((x, i) => ({{ x, y: transform(data.percentiles.p5[i]) }})),
      fill: false,
      borderColor: 'rgba(88, 166, 255, 0.2)',
      borderWidth: 1,
      pointRadius: 0,
    }},
    {{
      label: 'P25-P75 band',
      data: data.x_labels.map((x, i) => ({{ x, y: transform(data.percentiles.p75[i]) }})),
      fill: '+1',
      backgroundColor: 'rgba(88, 166, 255, 0.15)',
      borderColor: 'rgba(88, 166, 255, 0.3)',
      borderWidth: 1,
      pointRadius: 0,
    }},
    {{
      label: 'P25',
      data: data.x_labels.map((x, i) => ({{ x, y: transform(data.percentiles.p25[i]) }})),
      fill: false,
      borderColor: 'rgba(88, 166, 255, 0.3)',
      borderWidth: 1,
      pointRadius: 0,
    }},
    {{
      label: 'Median',
      data: data.x_labels.map((x, i) => ({{ x, y: transform(data.percentiles.p50[i]) }})),
      fill: false,
      borderColor: '#58a6ff',
      borderWidth: 2.5,
      pointRadius: 0,
    }},
    ...data.sample_paths.map((path, idx) => ({{
      label: 'Sample ' + (idx+1),
      data: data.x_labels.map((x, i) => ({{ x, y: transform(path[i] || 0) }})),
      fill: false,
      borderColor: `hsla(${{idx * 60 + 30}}, 70%, 60%, 0.3)`,
      borderWidth: 1,
      borderDash: [3, 3],
      pointRadius: 0,
    }})),
  ];
}}

function makeChart(canvasId, useLog) {{
  return new Chart(document.getElementById(canvasId), {{
    type: 'line',
    data: {{ datasets: makeDatasets(useLog) }},
    options: {{
      responsive: true,
      interaction: {{ mode: 'index', intersect: false }},
      plugins: {{
        title: {{
          display: true,
          text: useLog ? 'Equity Curve (log scale)' : 'Equity Curve (linear)',
          color: '#c9d1d9',
        }},
        legend: {{ display: false }},
        tooltip: {{
          callbacks: {{
            label: ctx => {{
              const val = useLog ? Math.pow(10, ctx.parsed.y) : ctx.parsed.y;
              return ctx.dataset.label + ': $' + val.toLocaleString(undefined, {{maximumFractionDigits: 0}});
            }}
          }}
        }},
      }},
      scales: {{
        x: {{
          type: 'linear',
          title: {{ display: true, text: 'Days', color: '#8b949e' }},
          ticks: {{ color: '#8b949e' }},
          grid: {{ color: '#21262d' }},
        }},
        y: {{
          title: {{ display: true, text: useLog ? 'Balance (log10)' : 'Balance ($)', color: '#8b949e' }},
          ticks: {{
            color: '#8b949e',
            callback: v => useLog ? '$' + Math.pow(10, v).toLocaleString(undefined, {{maximumFractionDigits: 0}}) : '$' + v.toLocaleString(),
          }},
          grid: {{ color: '#21262d' }},
        }},
      }},
    }},
  }});
}}

makeChart('fanChart', false);
makeChart('logChart', true);
</script>
</body>
</html>"""


def main():
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=str, default=str(DB_PATH))
    parser.add_argument("--output", type=str, default="backtesting/monte_carlo.html")
    parser.add_argument("--sims", type=int, default=2000)
    parser.add_argument("--months", type=int, default=6)
    parser.add_argument("--start", type=float, default=100.0)
    parser.add_argument("--move", type=float, default=0.08)
    parser.add_argument("--max-entry", type=float, default=0.55)
    parser.add_argument("--trades-per-day", type=float, default=10.0)
    args = parser.parse_args()

    logger.info("Extracting observed trades...")
    simulator = MonteCarloSimulator(Path(args.db))
    trades = simulator.extract_trades(args.move, args.max_entry)
    win_rate = sum(1 for t in trades if t.won) / len(trades) if trades else 0
    logger.info(f"Found {len(trades)} trades, WR={win_rate:.1%}")

    scenarios = [
        ("observed", win_rate),
        ("degraded_10", max(win_rate - 0.10, 0.5)),
        ("degraded_20", max(win_rate - 0.20, 0.5)),
    ]

    all_results = {}
    for label, wr in scenarios:
        logger.info(f"Simulating {label} (WR={wr:.0%})...")
        all_results[label] = run_simulation_paths(
            trades, wr,
            starting_balance=args.start,
            num_sims=args.sims,
            months=args.months,
            trades_per_day=args.trades_per_day,
        )

    # Use observed scenario for the main chart
    result = all_results["observed"]
    chart_data = {
        "x_labels": result["x_labels"],
        "percentiles": result["percentiles"],
        "sample_paths": result["sample_paths"],
    }

    html = HTML_TEMPLATE.format(
        chart_data_json=json.dumps(chart_data),
        num_sims=result["num_sims"],
        win_rate=result["win_rate"],
        trades_per_day=result["trades_per_day"],
        months=result["months"],
        starting_balance=result["starting_balance"],
    )

    out_path = Path(args.output)
    out_path.write_text(html)
    logger.info(f"Chart written to {out_path}")
    print(f"\nOpen in browser: file://{out_path.resolve()}")


if __name__ == "__main__":
    main()
