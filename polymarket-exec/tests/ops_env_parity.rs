use std::collections::HashMap;
use std::path::{Path, PathBuf};

use polymarket_exec::strategy_profile::StrategyProfile;

fn parse_env_example(raw: &str) -> HashMap<String, String> {
    raw.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            Some((key.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("polymarket-exec has repo parent")
        .to_path_buf()
}

#[test]
fn btc_5m_hybrid_env_example_matches_active_strategy_contract() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let env_path = manifest_dir.join("ops/env/btc_5m_hybrid.env.example");
    let raw = std::fs::read_to_string(&env_path).expect("example env should be readable");

    assert!(
        !raw.contains("WHALE_PAIR_"),
        "legacy WHALE_PAIR env prefix must not return"
    );
    assert!(
        !raw.contains("tlx_"),
        "provider API keys must stay out of tracked env examples"
    );

    let values = parse_env_example(&raw);
    assert_eq!(
        values.get("PM_BTC_5M_STRATEGY").map(String::as_str),
        Some("bonereaper_mm")
    );
    assert_eq!(
        values.get("PM_BTC_5M_PAPER_MODE").map(String::as_str),
        Some("true"),
        "tracked example must default to paper safety"
    );
    assert_eq!(
        values
            .get("PM_BTC_5M_MARKET_DISCOVERY_ENABLED")
            .map(String::as_str),
        Some("true"),
        "hybrid example should use dynamic market discovery instead of static token env"
    );
    assert!(
        values
            .get("POLYGON_RPC_URL")
            .is_some_and(|value| value.contains("telonex")),
        "example should document Telonex as the preferred Polygon RPC provider"
    );

    let profile_paths = values
        .get("PM_BTC_5M_STRATEGY_PROFILE_PATHS")
        .expect("hybrid example must define strategy profile paths");
    let root = repo_root();
    for profile_path in profile_paths.split(',').map(str::trim) {
        assert!(
            root.join(profile_path).exists(),
            "strategy profile path does not exist: {profile_path}"
        );
    }

    for yaml_owned_cap in [
        "PM_BTC_5M_EXEC_MAX_ORDER_NOTIONAL_USD",
        "PM_BTC_5M_EXEC_MAX_GROSS_NOTIONAL_USD",
        "PM_BTC_5M_EXEC_MAX_NET_NOTIONAL_PER_MARKET_USD",
        "PM_BTC_5M_EXEC_MAX_POSITION_QTY_PER_INSTRUMENT",
        "PM_BTC_5M_EXEC_MIN_FREE_CASH_USD",
        "PM_BTC_5M_EXEC_MIN_FREE_CASH_BPS",
        "PM_BTC_5M_EXEC_MAX_SESSION_LOSS_BPS",
        "PM_BTC_5M_EXEC_MAX_OPEN_ORDERS_TOTAL",
        "PM_BTC_5M_EXEC_MAX_OPEN_ORDERS_PER_MARKET",
    ] {
        assert!(
            !values.contains_key(yaml_owned_cap),
            "{yaml_owned_cap} must stay YAML-owned in bonereaper; leave env overrides commented"
        );
    }
}

#[test]
fn btc_5m_mm_tinylive_env_example_has_live_safety_and_no_secrets() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let env_path = manifest_dir.join("ops/env/btc_5m_mm_tinylive.env.example");
    let raw = std::fs::read_to_string(&env_path).expect("tiny-live env should be readable");

    assert!(
        !raw.contains("WHALE_PAIR_"),
        "legacy WHALE_PAIR env prefix must not return"
    );
    assert!(
        !raw.contains("tlx_"),
        "provider API keys must stay out of tracked env examples"
    );

    let values = parse_env_example(&raw);
    assert_eq!(
        values.get("PM_BTC_5M_STRATEGY").map(String::as_str),
        Some("paired_mm")
    );
    assert_eq!(
        values.get("PM_BTC_5M_PAPER_MODE").map(String::as_str),
        Some("false"),
        "tiny-live example should be explicit live mode"
    );
    assert_eq!(
        values
            .get("PM_BTC_5M_MARKET_DISCOVERY_ENABLED")
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        values.get("PM_BTC_5M_LIVE_POST_ONLY").map(String::as_str),
        Some("true")
    );
    assert!(
        values
            .get("POLYGON_RPC_URL")
            .is_some_and(|value| value.contains("telonex")),
        "tiny-live example should document Telonex as the preferred Polygon RPC provider"
    );
    for yaml_owned_cap in [
        "PM_BTC_5M_EXEC_MAX_ORDER_NOTIONAL_USD",
        "PM_BTC_5M_EXEC_MAX_GROSS_NOTIONAL_USD",
        "PM_BTC_5M_EXEC_MAX_NET_NOTIONAL_PER_MARKET_USD",
        "PM_BTC_5M_EXEC_MAX_POSITION_QTY_PER_INSTRUMENT",
        "PM_BTC_5M_EXEC_MIN_FREE_CASH_USD",
        "PM_BTC_5M_EXEC_MIN_FREE_CASH_BPS",
        "PM_BTC_5M_EXEC_MAX_SESSION_LOSS_BPS",
        "PM_BTC_5M_EXEC_MAX_OPEN_ORDERS_TOTAL",
        "PM_BTC_5M_EXEC_MAX_OPEN_ORDERS_PER_MARKET",
    ] {
        assert!(
            !values.contains_key(yaml_owned_cap),
            "{yaml_owned_cap} must stay YAML-owned in tinylive; leave env overrides commented"
        );
    }

    let profile_paths = values
        .get("PM_BTC_5M_STRATEGY_PROFILE_PATHS")
        .expect("tiny-live example must define strategy profile paths");
    let root = repo_root();
    for profile_path in profile_paths.split(',').map(str::trim) {
        assert!(
            root.join(profile_path).exists(),
            "strategy profile path does not exist: {profile_path}"
        );
    }
}

#[test]
fn btc_5m_bte_env_example_uses_overlay_profile_and_no_legacy_caps() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let env_path = manifest_dir.join("ops/env/btc_5m_bte.env.example");
    let raw = std::fs::read_to_string(&env_path).expect("BTE env should be readable");

    assert!(
        !raw.contains("WHALE_PAIR_"),
        "legacy WHALE_PAIR env prefix must not return"
    );
    assert!(
        !raw.contains("tlx_"),
        "provider API keys must stay out of tracked env examples"
    );

    let values = parse_env_example(&raw);
    assert_eq!(
        values.get("PM_BTC_5M_STRATEGY").map(String::as_str),
        Some("noop"),
        "BTE runs as an overlay; native StrategyMode should not run a legacy strategy underneath"
    );
    assert_eq!(
        values.get("PM_BTC_5M_BTE_SHADOW").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        values.get("PM_BTC_5M_BTE_PAPER_TRADE").map(String::as_str),
        Some("false"),
        "tracked BTE example should not submit paper orders unless explicitly armed"
    );
    assert_eq!(
        values.get("PM_BTC_5M_BTE_LIVE_TRADE").map(String::as_str),
        Some("false"),
        "tracked BTE example must not arm real-money submission"
    );
    assert_eq!(
        values.get("PM_BTC_5M_PAPER_MODE").map(String::as_str),
        Some("true"),
        "tracked BTE example must default to paper safety"
    );
    assert_eq!(
        values.get("PM_BTC_5M_EXEC_LOAD_DOTENV").map(String::as_str),
        Some("false"),
        "BTE sleeve should not load a stray local .env"
    );

    for yaml_owned_cap in [
        "PM_BTC_5M_EXEC_MAX_ORDER_NOTIONAL_USD",
        "PM_BTC_5M_EXEC_MAX_GROSS_NOTIONAL_USD",
        "PM_BTC_5M_EXEC_MAX_NET_NOTIONAL_PER_MARKET_USD",
        "PM_BTC_5M_EXEC_MAX_POSITION_QTY_PER_INSTRUMENT",
        "PM_BTC_5M_EXEC_MIN_FREE_CASH_USD",
        "PM_BTC_5M_EXEC_MIN_FREE_CASH_BPS",
        "PM_BTC_5M_EXEC_MAX_SESSION_LOSS_BPS",
        "PM_BTC_5M_EXEC_MAX_OPEN_ORDERS_TOTAL",
        "PM_BTC_5M_EXEC_MAX_OPEN_ORDERS_PER_MARKET",
        "PM_BTC_5M_BTE_MAX_ORDER_NOTIONAL_USD",
        "PM_BTC_5M_BTE_MAX_MARKET_NOTIONAL_USD",
    ] {
        assert!(
            !values.contains_key(yaml_owned_cap),
            "{yaml_owned_cap} must stay unset in the tracked BTE example"
        );
    }

    let profile_paths = values
        .get("PM_BTC_5M_STRATEGY_PROFILE_PATHS")
        .expect("BTE example must define a strategy profile path");
    let root = repo_root();
    for profile_path in profile_paths.split(',').map(str::trim) {
        let path = root.join(profile_path);
        assert!(
            path.exists(),
            "strategy profile path does not exist: {profile_path}"
        );
        let profile = StrategyProfile::load(&path).expect("BTE profile should parse");
        let bte = profile.back_to_explore_config();
        assert_eq!(profile.strategy.as_deref(), Some("back_to_explore"));
        assert_eq!(bte.clean_path_directional_clip_multiplier, 1.25);
        assert_eq!(bte.reversal_pressure_clip_multiplier, 0.0);
        assert_eq!(bte.reversal_pressure_directional_min_signal, 1.05);
        assert_eq!(bte.reversal_pressure_directional_min_edge, 0.01);
        assert_eq!(bte.reversal_pressure_directional_clip_multiplier, 0.85);
        assert_eq!(bte.range_repair_min_clip_multiplier, 0.70);
    }
}
