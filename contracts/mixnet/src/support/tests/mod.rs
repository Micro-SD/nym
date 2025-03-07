// Copyright 2021-2022 - Nym Technologies SA <contact@nymtech.net>
// SPDX-License-Identifier: Apache-2.0

#[cfg(test)]
pub mod fixtures;
#[cfg(test)]
pub mod messages;
#[cfg(test)]
pub mod queries;

#[cfg(test)]
pub mod test_helpers {
    use crate::constants;
    use crate::contract::instantiate;
    use crate::delegations::queries::query_mixnode_delegations_paged;
    use crate::delegations::storage as delegations_storage;
    use crate::delegations::transactions::try_delegate_to_mixnode;
    use crate::gateways::transactions::try_add_gateway;
    use crate::interval::transactions::{
        perform_pending_epoch_actions, perform_pending_interval_actions,
    };
    use crate::interval::{pending_events, storage as interval_storage};
    use crate::mixnet_contract_settings::storage as mixnet_params_storage;
    use crate::mixnet_contract_settings::storage::{
        minimum_mixnode_pledge, rewarding_denom, rewarding_validator_address,
    };
    use crate::mixnodes::storage as mixnodes_storage;
    use crate::mixnodes::transactions::{
        try_add_mixnode, try_add_mixnode_on_behalf, try_remove_mixnode,
    };
    use crate::rewards::storage as rewards_storage;
    use crate::rewards::transactions::try_reward_mixnode;
    use crate::support::tests;
    use crate::support::tests::fixtures::TEST_COIN_DENOM;
    use cosmwasm_std::testing::mock_dependencies;
    use cosmwasm_std::testing::mock_env;
    use cosmwasm_std::testing::mock_info;
    use cosmwasm_std::testing::MockApi;
    use cosmwasm_std::testing::MockQuerier;
    use cosmwasm_std::{Addr, BankMsg, CosmosMsg, Storage};
    use cosmwasm_std::{Coin, Order};
    use cosmwasm_std::{Decimal, Empty, MemoryStorage};
    use cosmwasm_std::{Deps, OwnedDeps};
    use cosmwasm_std::{DepsMut, MessageInfo};
    use cosmwasm_std::{Env, Response, Timestamp, Uint128};
    use mixnet_contract_common::events::{
        may_find_attribute, MixnetEventType, DELEGATES_REWARD_KEY, OPERATOR_REWARD_KEY,
    };
    use mixnet_contract_common::mixnode::{MixNodeRewarding, UnbondedMixnode};
    use mixnet_contract_common::pending_events::{PendingEpochEventData, PendingIntervalEventData};
    use mixnet_contract_common::reward_params::{Performance, RewardingParams};
    use mixnet_contract_common::rewarding::simulator::simulated_node::SimulatedNode;
    use mixnet_contract_common::rewarding::simulator::Simulator;
    use mixnet_contract_common::rewarding::RewardDistribution;
    use mixnet_contract_common::{
        Delegation, Gateway, InitialRewardingParams, InstantiateMsg, Interval, MixId, MixNode,
        Percent, RewardedSetNodeStatus,
    };
    use rand_chacha::rand_core::{CryptoRng, RngCore, SeedableRng};
    use rand_chacha::ChaCha20Rng;
    use std::time::Duration;

    pub fn assert_eq_with_leeway(a: Uint128, b: Uint128, leeway: Uint128) {
        if a > b {
            assert!(a - b <= leeway)
        } else {
            assert!(b - a <= leeway)
        }
    }

    pub fn assert_decimals(a: Decimal, b: Decimal) {
        let epsilon = Decimal::from_ratio(1u128, 100_000_000u128);
        if a > b {
            assert!(a - b < epsilon, "{} != {}", a, b)
        } else {
            assert!(b - a < epsilon, "{} != {}", a, b)
        }
    }

    pub struct TestSetup {
        pub deps: OwnedDeps<MemoryStorage, MockApi, MockQuerier<Empty>>,
        pub env: Env,
        pub rng: ChaCha20Rng,

        pub rewarding_validator: MessageInfo,
        pub owner: MessageInfo,
    }

    impl TestSetup {
        pub fn new() -> Self {
            let deps = init_contract();
            let rewarding_validator_address =
                rewarding_validator_address(deps.as_ref().storage).unwrap();
            let owner = mixnet_params_storage::CONTRACT_STATE
                .load(deps.as_ref().storage)
                .unwrap()
                .owner;

            TestSetup {
                deps,
                env: mock_env(),
                rng: test_rng(),
                rewarding_validator: mock_info(rewarding_validator_address.as_ref(), &[]),
                owner: mock_info(owner.as_str(), &[]),
            }
        }

        pub fn deps(&self) -> Deps<'_> {
            self.deps.as_ref()
        }

        pub fn deps_mut(&mut self) -> DepsMut<'_> {
            self.deps.as_mut()
        }

        pub fn env(&self) -> Env {
            self.env.clone()
        }

        pub fn rewarding_validator(&self) -> MessageInfo {
            self.rewarding_validator.clone()
        }

        pub fn rewarding_params(&self) -> RewardingParams {
            rewards_storage::REWARDING_PARAMS
                .load(self.deps().storage)
                .unwrap()
        }

        pub fn owner(&self) -> MessageInfo {
            self.owner.clone()
        }

        pub fn vesting_contract(&self) -> Addr {
            mixnet_params_storage::CONTRACT_STATE
                .load(self.deps().storage)
                .unwrap()
                .vesting_contract_address
        }

        pub fn current_interval(&self) -> Interval {
            interval_storage::current_interval(self.deps().storage).unwrap()
        }

        pub fn rewarded_set(&self) -> Vec<(MixId, RewardedSetNodeStatus)> {
            interval_storage::REWARDED_SET
                .range(self.deps().storage, None, None, Order::Ascending)
                .map(|res| res.unwrap())
                .collect::<Vec<_>>()
        }

        pub fn add_dummy_mixnode(&mut self, owner: &str, stake: Option<Uint128>) -> MixId {
            let stake = match stake {
                Some(amount) => {
                    let denom = rewarding_denom(self.deps().storage).unwrap();
                    Coin { denom, amount }
                }
                None => minimum_mixnode_pledge(self.deps.as_ref().storage).unwrap(),
            };

            let env = self.env();
            add_mixnode(&mut self.rng, self.deps.as_mut(), env, owner, vec![stake])
        }

        pub fn add_dummy_mixnode_with_proxy(
            &mut self,
            owner: &str,
            stake: Option<Uint128>,
            proxy: Addr,
        ) -> MixId {
            let stake = match stake {
                Some(amount) => {
                    let denom = rewarding_denom(self.deps().storage).unwrap();
                    Coin { denom, amount }
                }
                None => minimum_mixnode_pledge(self.deps.as_ref().storage).unwrap(),
            };

            let keypair = crypto::asymmetric::identity::KeyPair::new(&mut self.rng);
            let owner_signature = keypair
                .private_key()
                .sign(owner.as_bytes())
                .to_base58_string();

            let legit_sphinx_key = crypto::asymmetric::encryption::KeyPair::new(&mut self.rng);

            let info = mock_info(proxy.as_str(), &[stake]);
            let key = keypair.public_key().to_base58_string();
            let current_id_counter = mixnodes_storage::MIXNODE_ID_COUNTER
                .may_load(self.deps().storage)
                .unwrap()
                .unwrap_or_default();

            let env = self.env();
            try_add_mixnode_on_behalf(
                self.deps_mut(),
                env,
                info,
                MixNode {
                    identity_key: key,
                    sphinx_key: legit_sphinx_key.public_key().to_base58_string(),
                    ..tests::fixtures::mix_node_fixture()
                },
                tests::fixtures::mix_node_cost_params_fixture(),
                owner.to_string(),
                owner_signature,
            )
            .unwrap();

            // newly added mixnode gets assigned the current counter + 1
            current_id_counter + 1
        }

        pub fn start_unbonding_mixnode(&mut self, mix_id: MixId) {
            let bond_details = mixnodes_storage::mixnode_bonds()
                .load(self.deps().storage, mix_id)
                .unwrap();

            let env = self.env();
            try_remove_mixnode(
                self.deps_mut(),
                env,
                mock_info(bond_details.owner.as_str(), &[]),
            )
            .unwrap();
        }

        pub fn immediately_unbond_mixnode(&mut self, mix_id: MixId) {
            let env = self.env();
            pending_events::unbond_mixnode(self.deps_mut(), &env, env.block.height, mix_id)
                .unwrap();
        }

        pub fn add_immediate_delegation(
            &mut self,
            delegator: &str,
            amount: impl Into<Uint128>,
            target: MixId,
        ) {
            let denom = rewarding_denom(self.deps().storage).unwrap();
            let amount = Coin {
                denom,
                amount: amount.into(),
            };
            let env = self.env();
            pending_events::delegate(
                self.deps_mut(),
                &env,
                env.block.height,
                Addr::unchecked(delegator),
                target,
                amount,
                None,
            )
            .unwrap();
        }

        pub fn add_immediate_delegation_with_proxy(
            &mut self,
            delegator: &str,
            amount: impl Into<Uint128>,
            target: MixId,
            proxy: Addr,
        ) {
            let denom = rewarding_denom(self.deps().storage).unwrap();
            let amount = Coin {
                denom,
                amount: amount.into(),
            };
            let env = self.env();
            pending_events::delegate(
                self.deps_mut(),
                &env,
                env.block.height,
                Addr::unchecked(delegator),
                target,
                amount,
                Some(proxy),
            )
            .unwrap();
        }

        #[allow(unused)]
        pub fn add_delegation(
            &mut self,
            delegator: &str,
            amount: impl Into<Uint128>,
            target: MixId,
        ) {
            let denom = rewarding_denom(self.deps().storage).unwrap();
            let amount = Coin {
                denom,
                amount: amount.into(),
            };
            let env = self.env();
            delegate(self.deps_mut(), env, delegator, vec![amount], target)
        }

        pub fn remove_immediate_delegation(&mut self, delegator: &str, target: MixId) {
            let height = self.env.block.height;
            pending_events::undelegate(
                self.deps_mut(),
                height,
                Addr::unchecked(delegator),
                target,
                None,
            )
            .unwrap();
        }

        pub fn skip_to_next_epoch_end(&mut self) {
            self.skip_to_next_epoch();
            self.skip_to_current_epoch_end();
        }

        pub fn skip_to_current_epoch_end(&mut self) {
            let interval = interval_storage::current_interval(self.deps().storage).unwrap();
            let epoch_end = interval.current_epoch_end_unix_timestamp();
            // skip few blocks just in case
            self.env.block.height += 10;
            self.env.block.time = Timestamp::from_seconds(epoch_end as u64);
        }

        pub fn skip_to_current_interval_end(&mut self) {
            let interval = interval_storage::current_interval(self.deps().storage).unwrap();
            let interval_end = interval.current_interval_end_unix_timestamp();
            // skip few blocks just in case
            self.env.block.height += 10;
            self.env.block.time = Timestamp::from_seconds(interval_end as u64);
        }

        pub fn skip_to_next_epoch(&mut self) {
            let interval = interval_storage::current_interval(self.deps().storage).unwrap();
            let epoch_end = interval.current_epoch_end_unix_timestamp();
            // skip few blocks just in case
            self.env.block.height += 10;
            self.env.block.time = Timestamp::from_seconds(epoch_end as u64 + 1);
            let advanced = interval.advance_epoch();

            if interval.current_epoch_id() != interval.epochs_in_interval() {
                assert_eq!(interval.current_epoch_id() + 1, advanced.current_epoch_id())
            }

            interval_storage::save_interval(self.deps_mut().storage, &advanced).unwrap()
        }

        pub fn update_rewarded_set(&mut self, nodes: Vec<MixId>) {
            let active_set_size = rewards_storage::REWARDING_PARAMS
                .load(self.deps().storage)
                .unwrap()
                .active_set_size;
            interval_storage::update_rewarded_set(self.deps_mut().storage, active_set_size, nodes)
                .unwrap();
        }

        pub fn instantiate_simulator(&self, node: MixId) -> Simulator {
            simulator_from_single_node_state(self.deps(), node)
        }

        pub fn execute_all_pending_events(&mut self) {
            let env = self.env();
            execute_all_pending_events(self.deps_mut(), env)
        }

        pub fn pending_interval_events(&self) -> Vec<PendingIntervalEventData> {
            interval_storage::PENDING_INTERVAL_EVENTS
                .range(self.deps().storage, None, None, Order::Ascending)
                .map(|res| res.unwrap().1)
                .collect::<Vec<_>>()
        }

        pub fn pending_epoch_events(&self) -> Vec<PendingEpochEventData> {
            interval_storage::PENDING_EPOCH_EVENTS
                .range(self.deps().storage, None, None, Order::Ascending)
                .map(|res| res.unwrap().1)
                .collect::<Vec<_>>()
        }

        pub fn reward_with_distribution(
            &mut self,
            mix_id: MixId,
            performance: Performance,
        ) -> RewardDistribution {
            let env = self.env();
            let sender = self.rewarding_validator();

            let res =
                try_reward_mixnode(self.deps_mut(), env, sender, mix_id, performance).unwrap();
            let operator: Decimal = find_attribute(
                Some(MixnetEventType::MixnodeRewarding.to_string()),
                OPERATOR_REWARD_KEY,
                &res,
            )
            .parse()
            .unwrap();
            let delegates: Decimal = find_attribute(
                Some(MixnetEventType::MixnodeRewarding.to_string()),
                DELEGATES_REWARD_KEY,
                &res,
            )
            .parse()
            .unwrap();

            RewardDistribution {
                operator,
                delegates,
            }
        }

        pub fn read_delegation(
            &mut self,
            mix: MixId,
            owner: &str,
            proxy: Option<&str>,
        ) -> Delegation {
            read_delegation(
                self.deps().storage,
                mix,
                &Addr::unchecked(owner),
                &proxy.map(Addr::unchecked),
            )
            .unwrap()
        }

        pub fn mix_rewarding(&self, node: MixId) -> MixNodeRewarding {
            rewards_storage::MIXNODE_REWARDING
                .load(self.deps().storage, node)
                .unwrap()
        }

        pub fn delegation(&self, mix: MixId, owner: &str, proxy: &Option<Addr>) -> Delegation {
            read_delegation(self.deps().storage, mix, &Addr::unchecked(owner), proxy).unwrap()
        }
    }

    pub fn simulator_from_single_node_state(deps: Deps<'_>, node: MixId) -> Simulator {
        let mix_rewarding = rewards_storage::MIXNODE_REWARDING
            .load(deps.storage, node)
            .unwrap();
        let delegations = query_mixnode_delegations_paged(deps, node, None, None).unwrap();
        if delegations.delegations.len() as u32
            == constants::DELEGATION_PAGE_DEFAULT_RETRIEVAL_LIMIT
        {
            // can't be bothered to deal with paging for this test case since it's incredibly unlikely
            // we'd ever need it
            panic!("too many delegations")
        }
        let rewarding_params = rewards_storage::REWARDING_PARAMS
            .load(deps.storage)
            .unwrap();
        let interval = interval_storage::current_interval(deps.storage).unwrap();
        let mut simulator = Simulator::new(rewarding_params, interval);
        simulator.nodes.insert(
            0,
            SimulatedNode {
                mix_id: 0,
                rewarding_details: mix_rewarding,
                delegations: delegations
                    .delegations
                    .into_iter()
                    .map(|d| (d.owner.to_string(), d))
                    .collect(),
            },
        );

        simulator
    }

    pub fn get_bank_send_msg(response: &Response) -> Option<(String, Vec<Coin>)> {
        for msg in &response.messages {
            if let CosmosMsg::Bank(BankMsg::Send { to_address, amount }) = &msg.msg {
                return Some((to_address.clone(), amount.clone()));
            }
        }
        None
    }

    pub fn find_attribute<S: Into<String>>(
        event_type: Option<S>,
        attribute: &str,
        response: &Response,
    ) -> String {
        let event_type = event_type.map(Into::into);
        for event in &response.events {
            if let Some(typ) = &event_type {
                if &event.ty != typ {
                    continue;
                }
            }
            if let Some(attr) = may_find_attribute(event, attribute) {
                return attr;
            }
        }
        // this is only used in tests so panic here is fine
        panic!("did not find the attribute")
    }

    // using floats in tests is fine
    // (what it does is converting % value, like 12.34 into `Performance` (`Percent`)
    // which internally is represented by decimal `0.1234`
    pub fn performance(val: f32) -> Performance {
        assert!(val <= 100.0);
        assert!(val >= 0.0);

        // hehe, that's such a nasty conversion, but it works for test purposes
        let str = (val / 100.0).to_string();
        let dec = str.parse().unwrap();
        Performance::new(dec).unwrap()
    }

    // use rng with constant seed for all tests so that they would be deterministic
    pub fn test_rng() -> ChaCha20Rng {
        let dummy_seed = [42u8; 32];
        rand_chacha::ChaCha20Rng::from_seed(dummy_seed)
    }

    pub fn execute_all_pending_events(mut deps: DepsMut<'_>, env: Env) {
        perform_pending_epoch_actions(deps.branch(), &env, None).unwrap();
        perform_pending_interval_actions(deps.branch(), &env, None).unwrap();
    }

    pub fn mixnode_with_signature(
        mut rng: impl RngCore + CryptoRng,
        sender: &str,
    ) -> (MixNode, String) {
        let keypair = crypto::asymmetric::identity::KeyPair::new(&mut rng);
        let legit_sphinx_key = crypto::asymmetric::encryption::KeyPair::new(&mut rng);
        let owner_signature = keypair
            .private_key()
            .sign(sender.as_bytes())
            .to_base58_string();

        let identity_key = keypair.public_key().to_base58_string();
        let sphinx_key = legit_sphinx_key.public_key().to_base58_string();

        (
            MixNode {
                identity_key,
                sphinx_key,
                ..tests::fixtures::mix_node_fixture()
            },
            owner_signature,
        )
    }

    pub fn gateway_with_signature(
        mut rng: impl RngCore + CryptoRng,
        sender: &str,
    ) -> (Gateway, String) {
        let keypair = crypto::asymmetric::identity::KeyPair::new(&mut rng);
        let legit_sphinx_key = crypto::asymmetric::encryption::KeyPair::new(&mut rng);
        let owner_signature = keypair
            .private_key()
            .sign(sender.as_bytes())
            .to_base58_string();

        let identity_key = keypair.public_key().to_base58_string();
        let sphinx_key = legit_sphinx_key.public_key().to_base58_string();

        (
            Gateway {
                identity_key,
                sphinx_key,
                ..tests::fixtures::gateway_fixture()
            },
            owner_signature,
        )
    }

    pub fn add_dummy_delegations(mut deps: DepsMut<'_>, env: Env, mix_id: MixId, n: usize) {
        for i in 0..n {
            pending_events::delegate(
                deps.branch(),
                &env,
                env.block.height,
                Addr::unchecked(&format!("owner{}", i)),
                mix_id,
                tests::fixtures::good_mixnode_pledge().pop().unwrap(),
                None,
            )
            .unwrap();
        }
    }

    pub fn add_dummy_mixnodes(
        mut rng: impl RngCore + CryptoRng,
        mut deps: DepsMut<'_>,
        env: Env,
        n: usize,
    ) {
        for i in 0..n {
            add_mixnode(
                &mut rng,
                deps.branch(),
                env.clone(),
                &format!("owner{}", i),
                tests::fixtures::good_mixnode_pledge(),
            );
        }
    }

    pub fn add_dummy_gateways(
        mut rng: impl RngCore + CryptoRng,
        mut deps: DepsMut<'_>,
        env: Env,
        n: usize,
    ) {
        for i in 0..n {
            add_gateway(
                &mut rng,
                deps.branch(),
                env.clone(),
                &format!("owner{}", i),
                tests::fixtures::good_mixnode_pledge(),
            );
        }
    }

    pub fn add_dummy_unbonded_mixnodes(
        mut rng: impl RngCore + CryptoRng,
        mut deps: DepsMut<'_>,
        n: usize,
    ) {
        for i in 0..n {
            add_unbonded_mixnode(&mut rng, deps.branch(), None, &format!("owner{}", i));
        }
    }

    pub fn add_dummy_unbonded_mixnodes_with_owner(
        mut rng: impl RngCore + CryptoRng,
        mut deps: DepsMut<'_>,
        owner: &str,
        n: usize,
    ) {
        for _ in 0..n {
            add_unbonded_mixnode(&mut rng, deps.branch(), None, owner);
        }
    }

    pub fn add_dummy_unbonded_mixnodes_with_identity(
        mut rng: impl RngCore + CryptoRng,
        mut deps: DepsMut<'_>,
        identity: &str,
        n: usize,
    ) {
        for i in 0..n {
            add_unbonded_mixnode(
                &mut rng,
                deps.branch(),
                Some(identity),
                &format!("owner{}", i),
            );
        }
    }

    // same note as with `add_mixnode`
    pub fn add_unbonded_mixnode(
        mut rng: impl RngCore + CryptoRng,
        deps: DepsMut<'_>,
        identity_key: Option<&str>,
        owner: &str,
    ) -> MixId {
        let id = loop {
            let candidate = rng.next_u32();
            if !mixnodes_storage::unbonded_mixnodes().has(deps.storage, candidate) {
                break candidate;
            }
        };

        // we don't care about 'correctness' of the identity key here
        mixnodes_storage::unbonded_mixnodes()
            .save(
                deps.storage,
                id,
                &UnbondedMixnode {
                    identity_key: identity_key
                        .unwrap_or(&*format!("identity{}", id))
                        .to_string(),
                    owner: Addr::unchecked(owner),
                    proxy: None,
                    unbonding_height: 12345,
                },
            )
            .unwrap();

        id
    }

    // note to whoever wants to refactor this function, you dont want to grab rng here directly
    // via `let rng = test_rng()`
    // because it's extremely likely you might end up calling `add_mixnode()` multiple times
    // in the same test and thus you're going to get mixnodes with the same keys and that's
    // not what you want (presumably)
    pub fn add_mixnode(
        mut rng: impl RngCore + CryptoRng,
        deps: DepsMut<'_>,
        env: Env,
        sender: &str,
        stake: Vec<Coin>,
    ) -> MixId {
        let keypair = crypto::asymmetric::identity::KeyPair::new(&mut rng);
        let owner_signature = keypair
            .private_key()
            .sign(sender.as_bytes())
            .to_base58_string();

        let legit_sphinx_key = crypto::asymmetric::encryption::KeyPair::new(&mut rng);

        let info = mock_info(sender, &stake);
        let key = keypair.public_key().to_base58_string();
        let current_id_counter = mixnodes_storage::MIXNODE_ID_COUNTER
            .may_load(deps.storage)
            .unwrap()
            .unwrap_or_default();

        try_add_mixnode(
            deps,
            env,
            info,
            MixNode {
                identity_key: key,
                sphinx_key: legit_sphinx_key.public_key().to_base58_string(),
                ..tests::fixtures::mix_node_fixture()
            },
            tests::fixtures::mix_node_cost_params_fixture(),
            owner_signature,
        )
        .unwrap();

        // newly added mixnode gets assigned the current counter + 1
        current_id_counter + 1
    }

    // same note as with `add_mixnode`
    pub fn add_gateway(
        mut rng: impl RngCore + CryptoRng,
        deps: DepsMut<'_>,
        env: Env,
        sender: &str,
        stake: Vec<Coin>,
    ) -> String {
        let keypair = crypto::asymmetric::identity::KeyPair::new(&mut rng);
        let owner_signature = keypair
            .private_key()
            .sign(sender.as_bytes())
            .to_base58_string();

        let info = mock_info(sender, &stake);
        let key = keypair.public_key().to_base58_string();
        try_add_gateway(
            deps,
            env,
            info,
            Gateway {
                identity_key: key.clone(),
                ..tests::fixtures::gateway_fixture()
            },
            owner_signature,
        )
        .unwrap();
        key
    }

    fn initial_rewarding_params() -> InitialRewardingParams {
        let reward_pool = 250_000_000_000_000u128;
        let staking_supply = 100_000_000_000_000u128;

        InitialRewardingParams {
            initial_reward_pool: Decimal::from_atomics(reward_pool, 0).unwrap(), // 250M * 1M (we're expressing it all in base tokens)
            initial_staking_supply: Decimal::from_atomics(staking_supply, 0).unwrap(), // 100M * 1M
            sybil_resistance: Percent::from_percentage_value(30).unwrap(),
            active_set_work_factor: Decimal::from_atomics(10u32, 0).unwrap(),
            interval_pool_emission: Percent::from_percentage_value(2).unwrap(),
            rewarded_set_size: 240,
            active_set_size: 100,
        }
    }

    pub fn init_contract() -> OwnedDeps<MemoryStorage, MockApi, MockQuerier<Empty>> {
        let mut deps = mock_dependencies();
        let msg = InstantiateMsg {
            rewarding_validator_address: "rewarder".into(),
            vesting_contract_address: "vesting-contract".to_string(),
            rewarding_denom: TEST_COIN_DENOM.to_string(),
            epochs_in_interval: 720,
            epoch_duration: Duration::from_secs(60 * 60),
            initial_rewarding_params: initial_rewarding_params(),
        };
        let env = mock_env();
        let info = mock_info("creator", &[]);
        instantiate(deps.as_mut(), env, info, msg).unwrap();
        deps
    }

    pub fn delegate(deps: DepsMut<'_>, env: Env, sender: &str, stake: Vec<Coin>, mix_id: MixId) {
        let info = mock_info(sender, &stake);
        try_delegate_to_mixnode(deps, env, info, mix_id).unwrap();
    }

    pub(crate) fn read_delegation(
        storage: &dyn Storage,
        mix: MixId,
        owner: &Addr,
        proxy: &Option<Addr>,
    ) -> Option<Delegation> {
        delegations_storage::delegations()
            .may_load(
                storage,
                Delegation::generate_storage_key(mix, owner, proxy.as_ref()),
            )
            .unwrap()
    }
}
