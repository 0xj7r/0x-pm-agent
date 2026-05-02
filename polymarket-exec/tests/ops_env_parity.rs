use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
        Some("pair_cost_arb,paired_mm")
    );
    assert_eq!(
        values.get("PM_BTC_5M_PAPER_MODE").map(String::as_str),
        Some("true"),
        "tracked example must default to paper safety"
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
}
