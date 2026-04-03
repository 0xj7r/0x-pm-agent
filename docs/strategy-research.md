# Polymarket Strategy Research Report

**Date:** 2026-04-03
**Scope:** Evaluate alternative strategies beyond 5-min crypto latency arb
**Categories:** Weather markets, copy trading, other strategies, market microstructure

---

## Executive Summary

Weather markets are the clear winner. Polymarket hosts 490+ active weather markets across 20+ cities with daily temperature predictions. The edge is structural: ensemble weather forecasts (GFS 31-member, ECMWF) are 85-95% accurate at 1-3 day horizons, while retail-dominated market prices frequently misprice these outcomes. Documented bot profits range from $1.8K to $65K. The strategy fits our existing architecture perfectly.

**Recommendation priority:**
1. **Weather markets** (HIGH priority, build immediately)
2. **Cross-market arb** (MEDIUM priority, research further)
3. **Copy trading** (LOW priority, tools exist but edge is crowded)
4. **Other strategies** (LOW priority, most are well-known)

---

## 1. Weather Markets

### 1.1 Market Availability

Polymarket currently hosts **493 active weather markets** including:

- **Daily temperature markets** (325+ active): "Highest temperature in NYC on April 3?", "Highest temperature in London on April 3?"
- **Precipitation markets** (small but growing): "Precipitation in NYC in February?"
- **Global temperature anomaly markets** (140 active): "2026 March hottest on record?"

**Cities covered:** NYC, London, Miami, Chicago, Dallas, Istanbul, Buenos Aires, Wuhan, Atlanta, Hong Kong, Shanghai, Seoul, Toronto, Ankara, Auckland, Paris, Tokyo, Chongqing, Beijing, Shenzhen, Taipei, and more.

**Market format:** Multiple discrete temperature ranges per city per day. For example:
- Buenos Aires: 29C, 30C, 31C, 32C, 33C, 34C+
- Miami: 80-81F, 82-83F, etc.
- London: Celsius-based ranges

Markets resolve based on official weather station data (specific source cited in each market's rules).

### 1.2 Data Sources

| Provider | Free Tier | Models | Resolution | Update Freq | Best For |
|----------|-----------|--------|------------|-------------|----------|
| **Open-Meteo** | 10K calls/day (non-commercial) | GFS, ECMWF, ICON, AIFS | 9-25km | 1-6 hrs | Ensemble forecasts |
| **OpenWeatherMap** | 1K calls/day | Proprietary | Variable | Hourly | Simple forecasts |
| **Visual Crossing** | 1K records/day | Multiple | Variable | Hourly | Historical data |
| **Tomorrow.io** | 500 calls/day | Proprietary | 1-4km | Hourly | Hyperlocal |
| **NOAA** | Unlimited (US) | GFS, NAM, HRRR | 3-25km | 1-6 hrs | US cities |

**Key insight:** Open-Meteo provides free access to GFS 31-member ensemble forecasts and ECMWF IFS, which is the core data needed for probability estimation. No API key required for non-commercial use.

### 1.3 The Edge: Ensemble Forecasting vs Market Prices

The strategy exploits a structural inefficiency:

1. **Professional forecast accuracy:** 85-95% for 1-3 day temperature forecasts. ECMWF (the "Euro model") leads GFS by approximately 1 day of forecast skill. At 9km resolution, ECMWF is the most accurate model in 2026.

2. **Market pricing:** Retail-dominated, often mispriced. When 28/31 GFS ensemble members forecast above a threshold, the true probability is ~90%, but the market might price it at $0.15-0.40.

3. **Probability calculation method:**
   - Fetch 31-member GFS ensemble temperature forecasts from Open-Meteo
   - Count fraction of members above/below the market's temperature threshold
   - Compare model probability to market price
   - If edge > 8%, trade with Kelly criterion sizing

4. **Multi-model confirmation:** When GFS, ECMWF, and ICON all agree on a temperature range, probability rises to 70-90% depending on forecast horizon. Market prices often lag this consensus.

### 1.4 Existing Implementations

| Repo | Description | Approach |
|------|-------------|----------|
| [suislanchez/polymarket-kalshi-weather-bot](https://github.com/suislanchez/polymarket-kalshi-weather-bot) | Multi-platform weather bot | GFS 31-member ensemble, Kelly sizing, React dashboard. $1.8K highest profits. |
| [hcharper/polyBot-Weather](https://github.com/hcharper/polyBot-Weather) | Multi-strategy bot | Gaussian CDF with NOAA RMSE calibration. 25% Kelly cap. |
| [Degen Doppler](https://degendoppler.com/) | Edge finder tool | Compares forecast models to Polymarket prices, surfaces mispriced markets |
| [Thermometer](https://thermometer.josemaldona.do/) | Weather market dashboard | Visual tool for tracking weather market performance |

**Documented results:**
- One address: $1K to $24K since April 2025 on London weather markets
- Another bot: $65K profits across NYC, London, Seoul
- suislanchez repo: $1.8K highest simulation profits

### 1.5 Assessment

| Dimension | Rating | Notes |
|-----------|--------|-------|
| **Feasibility** | HIGH | Markets exist, APIs are free, strategy is well-documented |
| **Expected Edge** | 10-30% per trade | When ensemble consensus diverges from market price by >8% |
| **Data Availability** | HIGH | Open-Meteo (free), NOAA (free), multiple backup sources |
| **Implementation Complexity** | MEDIUM | Need ensemble fetching, probability calc, market matching, order execution |
| **Competition** | MEDIUM | Growing but not saturated; 493 markets across 20+ cities provide breadth |
| **Scalability** | HIGH | New cities/markets added regularly; can trade many markets simultaneously |

**VERDICT: BUILD THIS.**

---

## 2. Copy Trading / Social Signals

### 2.1 On-Chain Visibility

Polymarket runs on Polygon. Every wallet's complete trading history is visible on-chain, including:
- Full position history
- Win rate and total P&L
- Trade timing and sizing
- Market selection patterns

### 2.2 Available Tools

| Tool | Features | Pricing |
|------|----------|---------|
| [PolyTrack](https://www.polytrackhq.app/) | Whale profiles, cluster detection, leaderboards, alerts | Free core features |
| [Polywhaler](https://www.polywhaler.com/) | $10K+ trade monitoring, insider activity detection | Free |
| [Bravado](https://www.bravadotrade.com/) | Purpose-built Polymarket terminal, auto copy trading | Paid |
| [Polymarket Analytics](https://polymarketanalytics.com/) | Leaderboards, cross-platform data, 5-min updates | Free tier |
| [Polysights](https://polysights.xyz/) | AI-powered pattern detection | Free |
| [PolyAlertHub](https://polyalerthub.com/) | Real-time alerts via email/Telegram | Free tier |

**Curated resource list:** [Awesome-Prediction-Market-Tools](https://github.com/aarora4/Awesome-Prediction-Market-Tools) on GitHub catalogs 170+ tools across 19 categories.

### 2.3 Assessment

| Dimension | Rating | Notes |
|-----------|--------|-------|
| **Feasibility** | HIGH | On-chain data is fully transparent, tools exist |
| **Expected Edge** | LOW-MEDIUM | Alpha decays fast as more people copy the same whales |
| **Data Availability** | HIGH | On-chain data, multiple analytics platforms |
| **Implementation Complexity** | MEDIUM | Need on-chain monitoring, signal generation, execution |
| **Competition** | HIGH | Multiple mature tools already exist; crowded signal |

**Key concern:** Copy trading is inherently a crowded strategy. When multiple tools surface the same whale wallets, the edge compresses rapidly. The lag between whale trade and copy execution means worse prices. Additionally, sophisticated traders may use multiple wallets or intentionally mislead followers.

**VERDICT: Do not build. Use existing tools (PolyTrack, Polywhaler) for market intelligence, but do not build a dedicated copy trading system.**

---

## 3. Other High-Performing Strategies

### 3.1 Market Making

Several open-source market making bots exist:

| Repo | Description |
|------|-------------|
| [warproxxx/poly-maker](https://github.com/warproxxx/poly-maker) | Automated MM with Google Sheets config, both-side liquidity |
| [lorine93s/polymarket-market-maker-bot](https://github.com/lorine93s/polymarket-market-maker-bot) | Production-ready CLOB MM with inventory management, risk controls |

**Assessment:** Market making requires significant capital and sophistication. Polymarket's low fees (0.01% or $1 per $10K) make it viable, but the complexity of inventory management and adverse selection risk is high. Not aligned with our threshold-detection approach.

### 3.2 Binary Arbitrage (Complete-Set)

When YES + NO prices sum to less than $1.00, buying both sides guarantees profit.

- Example: YES=$0.48, NO=$0.48 -> Cost $0.96, payout $1.00 -> 4.2% profit
- [ent0n29/polybot](https://github.com/ent0n29/polybot) implements this for Up/Down binaries
- Mathematical edge, no prediction needed

**Assessment:** Opportunities are rare and fleeting. Automated bots already compete for these. Low expected volume.

### 3.3 AI Agent Strategies

[Polymarket/agents](https://github.com/Polymarket/agents) is the official developer framework for building AI agents on Polymarket. Uses LLMs to evaluate market questions and generate probability estimates.

**Assessment:** Interesting for long-horizon markets (politics, events) but not well-suited for high-frequency trading. LLM inference latency (seconds) makes this inappropriate for time-sensitive markets.

### 3.4 Sports Markets

Kalshi dominates sports betting with 90% of its volume from NFL, NBA, MLB. Polymarket has some sports markets but much lower liquidity.

**Assessment:** Sports analytics is a mature, competitive field. Building a competitive sports model requires deep domain expertise and extensive data pipelines. Not our comparative advantage.

### 3.5 Political / Event Markets

Poll-based strategies for political markets have shown success, but:
- Markets are seasonal (elections are periodic)
- Requires deep domain knowledge
- Long resolution times (months)
- Capital is locked for extended periods

**Assessment:** Not suitable for our short-horizon, automated approach.

### 3.6 News Sentiment

Using NLP/LLMs to parse news and trade on sentiment shifts. Theoretically promising but:
- Latency-sensitive (faster bots exist)
- Hard to calibrate
- Event-driven, not systematic

**Assessment:** Interesting but high implementation complexity with uncertain edge.

### 3.7 Multi-Strategy Bots

Notable multi-strategy implementations:
- [MrFadiAi/Polymarket-bot](https://github.com/MrFadiAi/Polymarket-bot): 4 strategies in one bot (v3.1, Jan 2026)
- [discountry/polymarket-trading-bot](https://github.com/discountry/polymarket-trading-bot): Monitors 15-min Up/Down markets for probability drops
- [echandsome/Polymarket-betting-bot](https://github.com/echandsome/Polymarket-betting-bot): TypeScript, copy trading + strategy bots

---

## 4. Market Microstructure Opportunities

### 4.1 Cross-Market Arbitrage (Polymarket vs Kalshi)

**The opportunity:** Same events priced differently on Polymarket and Kalshi.

**Platform comparison:**

| Feature | Polymarket | Kalshi |
|---------|-----------|--------|
| **Fees** | 0.01% ($1/10K) | ~1.2% ($120/10K) |
| **Weekly Volume** | ~$2.1B (47%) | ~$2.7B (53%) |
| **Strengths** | Politics, crypto, weather | Sports, weather |
| **Blockchain** | Polygon (on-chain) | Centralized (off-chain) |
| **API** | CLOB REST + WebSocket | REST with RSA-PSS auth |

**Key risk:** Resolution criteria may differ between platforms. The 2024 government shutdown case showed that Polymarket and Kalshi used different settlement standards for the "same" event, causing both sides to lose. 78% of low-volume arb opportunities failed in a 2025 study.

**Tools:**
- [AhaSignals research on cross-platform arb](https://ahasignals.com/research/prediction-market-arbitrage-strategies/)
- [Polymarket Analytics](https://polymarketanalytics.com/) has cross-platform comparison data
- suislanchez/polymarket-kalshi-weather-bot trades both platforms

**Assessment:** The fee differential (0.01% vs 1.2%) makes Polymarket-side arb more attractive. Weather markets specifically exist on both platforms (Kalshi KXHIGH series), making weather cross-market arb a natural extension of weather trading.

### 4.2 Longer Duration Crypto Markets

- 15-min Up/Down markets exist ([discountry/polymarket-trading-bot](https://github.com/discountry/polymarket-trading-bot) targets these)
- Longer crypto markets (hourly, daily) have different dynamics
- Less latency-sensitive but requires different signal generation

**Assessment:** Worth exploring as an extension of our existing crypto strategy. The hcharper/polyBot-Weather repo specifically targets "12hr-30 day markets" using Black-Scholes modeling for crypto prices.

### 4.3 Assessment

| Dimension | Rating | Notes |
|-----------|--------|-------|
| **Feasibility** | MEDIUM | Cross-market arb is feasible but risky due to resolution differences |
| **Expected Edge** | LOW-MEDIUM | Opportunities exist but are fleeting; high failure rate |
| **Data Availability** | HIGH | Both platforms have APIs |
| **Implementation Complexity** | HIGH | Need dual-platform integration, resolution matching, capital splitting |
| **Competition** | MEDIUM | Some bots exist but not saturated |

**VERDICT: Investigate weather cross-market arb (Polymarket + Kalshi) as a natural extension of weather strategy. Do not pursue general cross-market arb.**

---

## 5. Recommendations

### Tier 1: Build Now

**Weather Market Strategy**
- Use Open-Meteo GFS 31-member ensemble + ECMWF forecasts
- Target daily high temperature markets across all available cities
- Probability calculation: fraction of ensemble members above/below threshold
- Edge threshold: >8% divergence from market price
- Kelly criterion sizing (15-25% fractional Kelly)
- Start with London, NYC, Miami, Buenos Aires (highest liquidity)

### Tier 2: Build After Weather Is Validated

**Kalshi Weather Cross-Market Arb**
- Same ensemble model, applied to Kalshi KXHIGH series
- Exploit fee differential (0.01% Polymarket vs 1.2% Kalshi)
- Requires Kalshi API integration with RSA-PSS authentication

**Longer Duration Crypto**
- Apply Black-Scholes or volatility-based models to 12hr-30d crypto markets
- Extension of existing infrastructure

### Tier 3: Monitor Only

**Copy Trading**
- Use PolyTrack and Polywhaler for market intelligence
- Do not build automated copy trading (crowded, diminishing edge)

**Market Making**
- High capital requirements, complex inventory management
- Not aligned with our signal-based approach

---

## 6. Implementation Plan for Weather Strategy

### Architecture

```
clients/
  weather_api.py        # Open-Meteo ensemble + ECMWF client
  
strategies/
  weather.py            # WeatherStrategy: ensemble probability vs market price

shared/
  constants.py          # Add WEATHER_CITIES, ENSEMBLE_MEMBERS config

backtesting/
  weather_fetcher.py    # Historical forecast + actual temperature data
  weather_simulator.py  # Backtest weather strategy on historical data
```

### Data Pipeline

1. **Forecast ingestion:** Fetch GFS 31-member ensemble from Open-Meteo every 6 hours
2. **Market scanning:** Scan Polymarket weather markets via Gamma API
3. **Probability calculation:** Count ensemble members above/below each market threshold
4. **Edge detection:** Compare model probability to market price
5. **Order execution:** If edge > 8%, size with Kelly criterion and place order via CLOB

### Key APIs

- **Open-Meteo Ensemble API:** `https://ensemble-api.open-meteo.com/v1/ensemble?latitude={lat}&longitude={lon}&hourly=temperature_2m&models=gfs_seamless`
- **Open-Meteo Historical Forecast API:** For backtesting calibration
- **Polymarket Gamma API:** Market discovery (already implemented in our codebase)
- **NOAA:** Backup for US cities, includes historical RMSE data for calibration

### Risk Controls

- Maximum 5% bankroll per trade
- 8% minimum edge threshold
- Daily loss limit
- Maximum concurrent positions cap
- Ensemble agreement filter (require >70% member consensus before trading)

---

## Appendix: Source Links

### Weather Markets
- [Polymarket Weather Markets](https://polymarket.com/predictions/weather)
- [Polymarket Temperature Markets](https://polymarket.com/predictions/temperature)
- [Open-Meteo Ensemble API](https://open-meteo.com/en/docs/ensemble-api)
- [Open-Meteo GFS API](https://open-meteo.com/en/docs/gfs-api)

### Weather Bot Repos
- [suislanchez/polymarket-kalshi-weather-bot](https://github.com/suislanchez/polymarket-kalshi-weather-bot)
- [hcharper/polyBot-Weather](https://github.com/hcharper/polyBot-Weather)
- [Degen Doppler](https://degendoppler.com/)

### Copy Trading & Analytics
- [PolyTrack](https://www.polytrackhq.app/)
- [Polywhaler](https://www.polywhaler.com/)
- [Polymarket Analytics](https://polymarketanalytics.com/)
- [Awesome-Prediction-Market-Tools](https://github.com/aarora4/Awesome-Prediction-Market-Tools)

### Strategy Repos
- [Polymarket/agents](https://github.com/Polymarket/agents)
- [ent0n29/polybot](https://github.com/ent0n29/polybot)
- [warproxxx/poly-maker](https://github.com/warproxxx/poly-maker)
- [MrFadiAi/Polymarket-bot](https://github.com/MrFadiAi/Polymarket-bot)
- [discountry/polymarket-trading-bot](https://github.com/discountry/polymarket-trading-bot)

### Cross-Market Arb
- [AhaSignals: Cross-Platform Arb Strategies](https://ahasignals.com/research/prediction-market-arbitrage-strategies/)
- [Prediction Market Arb Guide 2026](https://newyorkcityservers.com/blog/prediction-market-arbitrage-guide)
- [Monad: Prediction Markets Cannot Agree](https://blog.monad.xyz/blog/prediction-market-arbitrage)

### Weather API Providers
- [Open-Meteo](https://open-meteo.com/)
- [OpenWeatherMap](https://openweathermap.org/)
- [Visual Crossing](https://www.visualcrossing.com/)
- [Tomorrow.io](https://www.tomorrow.io/)
- [NOAA](https://www.weather.gov/documentation/services-web-api)
