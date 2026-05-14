//! Deterministic walk-forward split planning for replay windows.
//!
//! The planner operates on `WindowPlan` rows from the backtest manifest and
//! produces chronological train/test/holdout boundaries. It does not run the
//! strategy itself; callers execute `backtest_runner` per split and compare
//! reports without letting calibration folds see the final holdout.

use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

use crate::replay::manifest::WindowPlan;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrainingMode {
    Expanding,
    Rolling,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkForwardConfig {
    pub min_train_windows: usize,
    pub test_windows: usize,
    pub step_windows: usize,
    pub holdout_windows: usize,
    pub training_mode: TrainingMode,
    pub rolling_train_windows: Option<usize>,
}

impl WalkForwardConfig {
    pub fn expanding(
        min_train_windows: usize,
        test_windows: usize,
        step_windows: usize,
        holdout_windows: usize,
    ) -> Self {
        Self {
            min_train_windows,
            test_windows,
            step_windows,
            holdout_windows,
            training_mode: TrainingMode::Expanding,
            rolling_train_windows: None,
        }
    }

    pub fn rolling(
        train_windows: usize,
        test_windows: usize,
        step_windows: usize,
        holdout_windows: usize,
    ) -> Self {
        Self {
            min_train_windows: train_windows,
            test_windows,
            step_windows,
            holdout_windows,
            training_mode: TrainingMode::Rolling,
            rolling_train_windows: Some(train_windows),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkForwardSplit {
    pub fold_index: usize,
    pub train_window_ids: Vec<String>,
    pub test_window_ids: Vec<String>,
    pub train_start_ns: i64,
    pub train_end_ns: i64,
    pub test_start_ns: i64,
    pub test_end_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkForwardPlan {
    pub splits: Vec<WalkForwardSplit>,
    pub holdout_window_ids: Vec<String>,
    pub holdout_start_ns: Option<i64>,
    pub holdout_end_ns: Option<i64>,
}

pub fn build_walk_forward_plan(
    windows: &[WindowPlan],
    cfg: &WalkForwardConfig,
) -> Result<WalkForwardPlan> {
    ensure!(cfg.min_train_windows > 0, "min_train_windows must be > 0");
    ensure!(cfg.test_windows > 0, "test_windows must be > 0");
    ensure!(cfg.step_windows > 0, "step_windows must be > 0");
    ensure!(
        cfg.holdout_windows < windows.len(),
        "holdout_windows must be smaller than available windows"
    );
    if cfg.training_mode == TrainingMode::Rolling {
        ensure!(
            cfg.rolling_train_windows.unwrap_or(0) >= cfg.min_train_windows,
            "rolling_train_windows must be >= min_train_windows"
        );
    }

    let mut ordered = windows.to_vec();
    ordered.sort_by(|a, b| {
        (a.start_ns, a.end_ns, &a.window_id).cmp(&(b.start_ns, b.end_ns, &b.window_id))
    });
    ensure!(
        ordered
            .windows(2)
            .all(|pair| pair[0].end_ns <= pair[1].start_ns),
        "walk-forward windows must be non-overlapping and chronological"
    );

    let calibration_len = ordered.len() - cfg.holdout_windows;
    ensure!(
        calibration_len >= cfg.min_train_windows + cfg.test_windows,
        "not enough non-holdout windows for one train/test split"
    );

    let mut splits = Vec::new();
    let mut train_end = cfg.min_train_windows;
    while train_end + cfg.test_windows <= calibration_len {
        let train_start = match cfg.training_mode {
            TrainingMode::Expanding => 0,
            TrainingMode::Rolling => train_end.saturating_sub(
                cfg.rolling_train_windows
                    .expect("rolling train windows validated"),
            ),
        };
        let test_start = train_end;
        let test_end = test_start + cfg.test_windows;
        let train = &ordered[train_start..train_end];
        let test = &ordered[test_start..test_end];
        splits.push(WalkForwardSplit {
            fold_index: splits.len(),
            train_window_ids: train
                .iter()
                .map(|window| window.window_id.clone())
                .collect(),
            test_window_ids: test.iter().map(|window| window.window_id.clone()).collect(),
            train_start_ns: train.first().map(|window| window.start_ns).unwrap_or(0),
            train_end_ns: train.last().map(|window| window.end_ns).unwrap_or(0),
            test_start_ns: test.first().map(|window| window.start_ns).unwrap_or(0),
            test_end_ns: test.last().map(|window| window.end_ns).unwrap_or(0),
        });
        train_end += cfg.step_windows;
    }

    let holdout = &ordered[calibration_len..];
    Ok(WalkForwardPlan {
        splits,
        holdout_window_ids: holdout
            .iter()
            .map(|window| window.window_id.clone())
            .collect(),
        holdout_start_ns: holdout.first().map(|window| window.start_ns),
        holdout_end_ns: holdout.last().map(|window| window.end_ns),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows(n: usize) -> Vec<WindowPlan> {
        (0..n)
            .map(|i| WindowPlan {
                window_id: format!("btc_5m/2026-05-01/w{i}"),
                start_ns: i as i64 * 300,
                end_ns: (i as i64 + 1) * 300,
            })
            .collect()
    }

    #[test]
    fn expanding_plan_keeps_final_holdout_unseen() {
        let plan = build_walk_forward_plan(&windows(10), &WalkForwardConfig::expanding(4, 2, 2, 2))
            .unwrap();

        assert_eq!(
            plan.holdout_window_ids,
            vec!["btc_5m/2026-05-01/w8", "btc_5m/2026-05-01/w9"]
        );
        assert_eq!(plan.splits.len(), 2);
        assert_eq!(
            plan.splits[0].train_window_ids,
            vec![
                "btc_5m/2026-05-01/w0",
                "btc_5m/2026-05-01/w1",
                "btc_5m/2026-05-01/w2",
                "btc_5m/2026-05-01/w3"
            ]
        );
        assert_eq!(
            plan.splits[0].test_window_ids,
            vec!["btc_5m/2026-05-01/w4", "btc_5m/2026-05-01/w5"]
        );
        assert!(plan.splits.iter().all(|split| split
            .test_window_ids
            .iter()
            .all(|id| !plan.holdout_window_ids.contains(id))));
    }

    #[test]
    fn rolling_plan_limits_training_history() {
        let plan =
            build_walk_forward_plan(&windows(12), &WalkForwardConfig::rolling(3, 2, 2, 1)).unwrap();

        assert_eq!(plan.splits[0].train_window_ids.len(), 3);
        assert_eq!(plan.splits[1].train_window_ids.len(), 3);
        assert_eq!(
            plan.splits[1].train_window_ids,
            vec![
                "btc_5m/2026-05-01/w2",
                "btc_5m/2026-05-01/w3",
                "btc_5m/2026-05-01/w4"
            ]
        );
    }

    #[test]
    fn rejects_overlapping_windows() {
        let mut ws = windows(5);
        ws[2].start_ns = ws[1].start_ns;

        let err =
            build_walk_forward_plan(&ws, &WalkForwardConfig::expanding(2, 1, 1, 1)).unwrap_err();

        assert!(err.to_string().contains("non-overlapping"));
    }
}
