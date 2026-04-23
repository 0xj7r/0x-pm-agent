# Strategy Report: unlawful-shear

Wallet: `0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82`
Focus family: `btc-updown-5m-`

## Key Takeaways
- The wallet is a high-frequency two-sided recycler, not a one-shot directional sniper: 334 of 370 historical markets are two-sided, and 341 show merge activity.
- The regime has clearly specialized into BTC 5m. Early history was broader ([['BTC', 21709], ['ETH', 2088], ['SOL', 428], ['XRP', 275]]) but the latest phase is fully BTC with [['5m', 21000]].
- Entry timing is concentrated in the middle of the 5m window rather than purely at the open or final seconds: median first buy offset is 12s and median last buy offset is 294s.
- Execution is mixed. Recent joined rows are roughly 1779 passive vs 1504 taker-classified buys, which implies active completion rather than passive-only posting.
- Visible negative-risk windows are rare, so the edge is not just naive ask-sum<1 scanning: 9 of 3291 recent buy rows had ask_sum<1.
- Sizing is fragmented but material. Recent BTC 5m child clips have median $2.95 notional and p90 $31.00, with 196 exact-100-share clips.
- Closed-position behavior is consistent with paired inventory management: 24 of 26 sampled closed markets contain both legs.

## Historical Shape
- Rows: 119000
- Markets: 370
- Two-sided markets: 334
- Merge markets: 341
- Asset mix: {'BTC': 105875, 'ETH': 7583, 'SOL': 2895, 'XRP': 2348, 'OTHER': 162, 'BNB': 137}
- Family mix: {'updown_5m': 109483, 'updown_15m': 9139, 'updown_other': 378}

## Recent Entry Pattern
- Buy rows: 3260
- Markets: 9
- Median clip USD: 2.94825
- P90 clip USD: 31.0
- Median first buy offset sec: 12.0
- Median last buy offset sec: 294.0
- Median fills per market: 397.0

## Recent Market Conditions
- Execution mix: {'likely_maker_or_passive': 1779, 'likely_taker': 1504, 'unknown': 8}
- Fill quality: {'inside_or_better_than_ask': 1779, 'through_ask': 956, 'at_ask': 548, 'unknown': 8}
- Bucket counts: {'120_180': 751, '60_120': 868, '0_60': 746, '180_240': 741, '240_300': 182, 'late': 3}
- Mean ask sum: 1.0112675626145389
- Ask sum < 1 count: 9
- Maker fill-vs-ask mean: -0.03537599114304326
- Taker fill-vs-ask mean: 0.026981411763080304

## Closed Positions
- Closed markets sampled: 26
- Paired closed markets: 24
- Total realized PnL: 1097.7119539999999
- Win rate: 0.6153846153846154
- Median total bought per closed market: 7400.0569775
