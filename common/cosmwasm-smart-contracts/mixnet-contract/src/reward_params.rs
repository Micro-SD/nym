// Copyright 2022 - Nym Technologies SA <contact@nymtech.net>
// SPDX-License-Identifier: Apache-2.0

use crate::helpers::IntoBaseDecimal;
use crate::{error::MixnetContractError, Percent};
use cosmwasm_std::Decimal;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub type Performance = Percent;

/// Parameters required by the mix-mining reward distribution that do not change during an interval.
#[cfg_attr(feature = "generate-ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "generate-ts",
    ts(export_to = "ts-packages/types/src/types/rust/IntervalRewardParams.ts")
)]
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Serialize, JsonSchema)]
pub struct IntervalRewardParams {
    /// Current value of the rewarding pool.
    /// It is expected to be constant throughout the interval.
    #[cfg_attr(feature = "generate-ts", ts(type = "string"))]
    pub reward_pool: Decimal,

    /// Current value of the staking supply.
    /// It is expected to be constant throughout the interval.
    #[cfg_attr(feature = "generate-ts", ts(type = "string"))]
    pub staking_supply: Decimal,

    // computed values
    /// Current value of the computed reward budget per epoch, per node.
    /// It is expected to be constant throughout the interval.
    #[cfg_attr(feature = "generate-ts", ts(type = "string"))]
    pub epoch_reward_budget: Decimal,

    /// Current value of the stake saturation point.
    /// It is expected to be constant throughout the interval.
    #[cfg_attr(feature = "generate-ts", ts(type = "string"))]
    pub stake_saturation_point: Decimal,

    // constants(-ish)
    // default: 30%
    /// Current value of the sybil resistance percent (`alpha`).
    /// It is not really expected to be changing very often.
    /// As a matter of fact, unless there's a very specific reason, it should remain constant.
    #[cfg_attr(feature = "generate-ts", ts(type = "string"))]
    pub sybil_resistance: Percent,

    // default: 10
    /// Current active set work factor.
    /// It is not really expected to be changing very often.
    /// As a matter of fact, unless there's a very specific reason, it should remain constant.
    #[cfg_attr(feature = "generate-ts", ts(type = "string"))]
    pub active_set_work_factor: Decimal,

    // default: 2%
    /// Current maximum interval pool emission.
    /// Assuming all nodes in the rewarded set are fully saturated and have 100% performance,
    /// this % of the reward pool would get distributed in rewards to all operators and its delegators.
    /// It is not really expected to be changing very often.
    /// As a matter of fact, unless there's a very specific reason, it should remain constant.
    #[cfg_attr(feature = "generate-ts", ts(type = "string"))]
    pub interval_pool_emission: Percent,
}

impl IntervalRewardParams {
    pub fn to_inline_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "serialisation failure".into())
    }
}

#[cfg_attr(feature = "generate-ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "generate-ts",
    ts(export_to = "ts-packages/types/src/types/rust/RewardingParams.ts")
)]
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Serialize, JsonSchema)]
pub struct RewardingParams {
    /// Parameters that should remain unchanged throughout an interval.
    pub interval: IntervalRewardParams,

    // while the active set size can change between epochs to accommodate for bandwidth demands,
    // the active set size should be unchanged between epochs and should only be adjusted between
    // intervals. However, it makes more sense to keep both of those values together as they're
    // very strongly related to each other.
    pub rewarded_set_size: u32,
    pub active_set_size: u32,
}

impl RewardingParams {
    pub fn active_node_work(&self) -> Decimal {
        self.interval.active_set_work_factor * self.standby_node_work()
    }

    pub fn standby_node_work(&self) -> Decimal {
        let f = self.interval.active_set_work_factor;
        let k = self.dec_rewarded_set_size();
        let one = Decimal::one();

        // nodes in reserve
        let k_r = self.dec_standby_set_size();

        one / (f * k - (f - one) * k_r)
    }

    pub fn dec_rewarded_set_size(&self) -> Decimal {
        // the unwrap here is fine as we're guaranteed an `u32` is going to fit in a Decimal
        // with 0 decimal places
        #[allow(clippy::unwrap_used)]
        self.rewarded_set_size.into_base_decimal().unwrap()
    }

    pub fn dec_active_set_size(&self) -> Decimal {
        // the unwrap here is fine as we're guaranteed an `u32` is going to fit in a Decimal
        // with 0 decimal places
        #[allow(clippy::unwrap_used)]
        self.active_set_size.into_base_decimal().unwrap()
    }

    fn dec_standby_set_size(&self) -> Decimal {
        // the unwrap here is fine as we're guaranteed an `u32` is going to fit in a Decimal
        // with 0 decimal places
        #[allow(clippy::unwrap_used)]
        (self.rewarded_set_size - self.active_set_size)
            .into_base_decimal()
            .unwrap()
    }

    pub fn apply_epochs_in_interval_change(&mut self, new_epochs_in_interval: u32) {
        // the unwrap here is fine as we're guaranteed an `u32` is going to fit in a Decimal
        // with 0 decimal places
        #[allow(clippy::unwrap_used)]
        let new_epochs_in_interval = new_epochs_in_interval.into_base_decimal().unwrap();

        self.interval.epoch_reward_budget = self.interval.reward_pool / new_epochs_in_interval
            * self.interval.interval_pool_emission;
    }

    pub fn try_change_active_set_size(
        &mut self,
        new_active_set_size: u32,
    ) -> Result<(), MixnetContractError> {
        if new_active_set_size == 0 {
            return Err(MixnetContractError::ZeroActiveSet);
        }

        if new_active_set_size > self.rewarded_set_size {
            return Err(MixnetContractError::InvalidActiveSetSize);
        }

        self.active_set_size = new_active_set_size;
        Ok(())
    }

    pub fn try_apply_updates(
        &mut self,
        updates: IntervalRewardingParamsUpdate,
        epochs_in_interval: u32,
    ) -> Result<(), MixnetContractError> {
        if !updates.contains_updates() {
            return Err(MixnetContractError::EmptyParamsChangeMsg);
        }

        let mut recompute_epoch_budget = false;
        let mut recompute_saturation_point = false;

        if let Some(reward_pool) = updates.reward_pool {
            recompute_epoch_budget = true;
            self.interval.reward_pool = reward_pool;
        }

        if let Some(staking_supply) = updates.staking_supply {
            recompute_saturation_point = true;
            self.interval.staking_supply = staking_supply;
        }

        if let Some(sybil_resistance_percent) = updates.sybil_resistance_percent {
            self.interval.sybil_resistance = sybil_resistance_percent;
        }

        if let Some(active_set_work_factor) = updates.active_set_work_factor {
            self.interval.active_set_work_factor = active_set_work_factor;
        }

        if let Some(interval_pool_emission) = updates.interval_pool_emission {
            recompute_epoch_budget = true;
            self.interval.interval_pool_emission = interval_pool_emission;
        }

        if let Some(rewarded_set_size) = updates.rewarded_set_size {
            if rewarded_set_size == 0 {
                return Err(MixnetContractError::ZeroRewardedSet);
            }
            if rewarded_set_size < self.active_set_size {
                return Err(MixnetContractError::InvalidRewardedSetSize);
            }

            recompute_saturation_point = true;
            self.rewarded_set_size = rewarded_set_size;
        }

        if recompute_epoch_budget {
            self.interval.epoch_reward_budget = self.interval.reward_pool
                / epochs_in_interval.into_base_decimal()?
                * self.interval.interval_pool_emission;
        }

        if recompute_saturation_point {
            self.interval.stake_saturation_point =
                self.interval.staking_supply / self.rewarded_set_size.into_base_decimal()?
        }

        Ok(())
    }
}

// TODO: possibly refactor this
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Serialize, JsonSchema)]
pub struct NodeRewardParams {
    pub performance: Percent,
    pub in_active_set: bool,
}

impl NodeRewardParams {
    pub fn new(performance: Percent, in_active_set: bool) -> Self {
        NodeRewardParams {
            performance,
            in_active_set,
        }
    }
}

#[cfg_attr(feature = "generate-ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "generate-ts",
    ts(export_to = "ts-packages/types/src/types/rust/IntervalRewardingParamsUpdate.ts")
)]
#[derive(
    Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, PartialOrd, Serialize, JsonSchema,
)]
pub struct IntervalRewardingParamsUpdate {
    #[cfg_attr(feature = "generate-ts", ts(type = "string | null"))]
    pub reward_pool: Option<Decimal>,

    #[cfg_attr(feature = "generate-ts", ts(type = "string | null"))]
    pub staking_supply: Option<Decimal>,

    #[cfg_attr(feature = "generate-ts", ts(type = "string | null"))]
    pub sybil_resistance_percent: Option<Percent>,

    #[cfg_attr(feature = "generate-ts", ts(type = "string | null"))]
    pub active_set_work_factor: Option<Decimal>,

    #[cfg_attr(feature = "generate-ts", ts(type = "string | null"))]
    pub interval_pool_emission: Option<Percent>,

    pub rewarded_set_size: Option<u32>,
}

impl IntervalRewardingParamsUpdate {
    pub fn contains_updates(&self) -> bool {
        // essentially at least a single field has to be a `Some`
        self.reward_pool.is_some()
            || self.staking_supply.is_some()
            || self.sybil_resistance_percent.is_some()
            || self.active_set_work_factor.is_some()
            || self.interval_pool_emission.is_some()
            || self.rewarded_set_size.is_some()
    }

    pub fn to_inline_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "serialisation failure".into())
    }
}
