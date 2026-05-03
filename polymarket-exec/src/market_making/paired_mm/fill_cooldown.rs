//! Post-fill cooldown policy.
//!
//! After one leg fills alone, immediately regenerating the full paired ladder
//! can compound the exact one-sided inventory problem we are trying to avoid.
//! This small state machine suppresses new paired entries for a short window
//! while rescue/merge logic catches up.

use crate::market_making::pairing::types::LadderLeg;
use crate::types::EpochMillis;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FillCooldownConfig {
    pub post_fill_cooldown_ms: u64,
    pub asymmetric_fill_cooldown_ms: u64,
}

impl Default for FillCooldownConfig {
    fn default() -> Self {
        Self {
            post_fill_cooldown_ms: 5_000,
            asymmetric_fill_cooldown_ms: 10_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FillCooldown {
    last_yes_fill_ms: Option<EpochMillis>,
    last_no_fill_ms: Option<EpochMillis>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FillCooldownDecision {
    Allow,
    SuppressPaired { reason: String },
}

impl FillCooldown {
    pub fn record_fill(&mut self, leg: LadderLeg, observed_at_ms: EpochMillis) {
        match leg {
            LadderLeg::Yes => self.last_yes_fill_ms = Some(observed_at_ms),
            LadderLeg::No => self.last_no_fill_ms = Some(observed_at_ms),
        }
    }

    pub fn decision(
        &self,
        now_ms: EpochMillis,
        config: FillCooldownConfig,
    ) -> FillCooldownDecision {
        let yes_age = self.last_yes_fill_ms.map(|ts| now_ms.saturating_sub(ts));
        let no_age = self.last_no_fill_ms.map(|ts| now_ms.saturating_sub(ts));

        let recent_yes = yes_age.is_some_and(|age| age < config.asymmetric_fill_cooldown_ms);
        let recent_no = no_age.is_some_and(|age| age < config.asymmetric_fill_cooldown_ms);
        if recent_yes ^ recent_no {
            return FillCooldownDecision::SuppressPaired {
                reason: format!(
                    "asymmetric fill cooldown yes_age_ms={yes_age:?} no_age_ms={no_age:?}"
                ),
            };
        }

        let recent_any = yes_age
            .into_iter()
            .chain(no_age)
            .any(|age| age < config.post_fill_cooldown_ms);
        if recent_any {
            return FillCooldownDecision::SuppressPaired {
                reason: "post-fill cooldown".to_string(),
            };
        }

        FillCooldownDecision::Allow
    }
}
