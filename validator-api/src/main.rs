// Copyright 2020 - Nym Technologies SA <contact@nymtech.net>
// SPDX-License-Identifier: Apache-2.0

#[macro_use]
extern crate rocket;

use crate::config::Config;
use crate::contract_cache::ValidatorCacheRefresher;
use crate::network_monitor::NetworkMonitorBuilder;
use crate::node_status_api::uptime_updater::HistoricalUptimeUpdater;
use crate::nymd_client::Client;
use crate::storage::ValidatorApiStorage;
use ::config::defaults::mainnet::read_var_if_not_default;
use ::config::defaults::setup_env;
#[cfg(feature = "coconut")]
use ::config::defaults::var_names::API_VALIDATOR;
use ::config::defaults::var_names::{CONFIGURED, MIXNET_CONTRACT_ADDRESS, MIX_DENOM};
use ::config::NymConfig;
use anyhow::Result;
use clap::{crate_version, App, Arg, ArgMatches};
use contract_cache::ValidatorCache;
use log::{info, warn};
use node_status_api::NodeStatusCache;
use okapi::openapi3::OpenApi;
use rocket::fairing::AdHoc;
use rocket::http::Method;
use rocket::{Ignite, Rocket};
use rocket_cors::{AllowedHeaders, AllowedOrigins, Cors};
use rocket_okapi::mount_endpoints_and_merged_docs;
use rocket_okapi::swagger_ui::make_swagger_ui;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::{fs, process};
use task::ShutdownNotifier;
use tokio::sync::Notify;
use validator_client::nymd::SigningNymdClient;

use crate::epoch_operations::RewardedSetUpdater;
#[cfg(feature = "coconut")]
use coconut::{comm::QueryCommunicationChannel, InternalSignRequest};
#[cfg(feature = "coconut")]
use coconut_interface::{Base58, KeyPair};

pub(crate) mod config;
pub(crate) mod contract_cache;
mod epoch_operations;
mod network_monitor;
mod node_status_api;
pub(crate) mod nymd_client;
pub(crate) mod storage;
mod swagger;

#[cfg(feature = "coconut")]
mod coconut;

const ID: &str = "id";
const CONFIG_ENV_FILE: &str = "config-env-file";
const MONITORING_ENABLED: &str = "enable-monitor";
const REWARDING_ENABLED: &str = "enable-rewarding";
const MIXNET_CONTRACT_ARG: &str = "mixnet-contract";
const MNEMONIC_ARG: &str = "mnemonic";
const WRITE_CONFIG_ARG: &str = "save-config";
const NYMD_VALIDATOR_ARG: &str = "nymd-validator";
const ENABLED_CREDENTIALS_MODE_ARG_NAME: &str = "enabled-credentials-mode";

#[cfg(feature = "coconut")]
const API_VALIDATORS_ARG: &str = "api-validators";
#[cfg(feature = "coconut")]
const KEYPAIR_ARG: &str = "keypair";
#[cfg(feature = "coconut")]
const COCONUT_ENABLED: &str = "enable-coconut";

const REWARDING_MONITOR_THRESHOLD_ARG: &str = "monitor-threshold";

const MIN_MIXNODE_RELIABILITY_ARG: &str = "min_mixnode_reliability";
const MIN_GATEWAY_RELIABILITY_ARG: &str = "min_gateway_reliability";

fn long_version() -> String {
    format!(
        r#"
{:<20}{}
{:<20}{}
{:<20}{}
{:<20}{}
{:<20}{}
{:<20}{}
{:<20}{}
{:<20}{}
"#,
        "Build Timestamp:",
        env!("VERGEN_BUILD_TIMESTAMP"),
        "Build Version:",
        env!("VERGEN_BUILD_SEMVER"),
        "Commit SHA:",
        env!("VERGEN_GIT_SHA"),
        "Commit Date:",
        env!("VERGEN_GIT_COMMIT_TIMESTAMP"),
        "Commit Branch:",
        env!("VERGEN_GIT_BRANCH"),
        "rustc Version:",
        env!("VERGEN_RUSTC_SEMVER"),
        "rustc Channel:",
        env!("VERGEN_RUSTC_CHANNEL"),
        "cargo Profile:",
        env!("VERGEN_CARGO_PROFILE")
    )
}

fn parse_args() -> ArgMatches {
    let build_details = long_version();
    let base_app = App::new("Nym Validator API")
        .version(crate_version!())
        .long_version(&*build_details)
        .author("Nymtech")
        .arg(
            Arg::with_name(CONFIG_ENV_FILE)
                .help("Path pointing to an env file that configures the validator API")
                .long(CONFIG_ENV_FILE)
                .takes_value(true)
        )
        .arg(
            Arg::with_name(ID)
                .help("Id of the validator-api we want to run")
                .long(ID)
                .takes_value(true)
        )
        .arg(
            Arg::with_name(MONITORING_ENABLED)
                .help("specifies whether a network monitoring is enabled on this API")
                .long(MONITORING_ENABLED)
                .short('m')
        )
        .arg(
            Arg::with_name(REWARDING_ENABLED)
                .help("specifies whether a network rewarding is enabled on this API")
                .long(REWARDING_ENABLED)
                .short('r')
                .requires_all(&[MONITORING_ENABLED, MNEMONIC_ARG])
        )
        .arg(
            Arg::with_name(NYMD_VALIDATOR_ARG)
                .help("Endpoint to nymd part of the validator from which the monitor will grab nodes to test")
                .long(NYMD_VALIDATOR_ARG)
                .takes_value(true)
        )
        .arg(Arg::with_name(MIXNET_CONTRACT_ARG)
                 .long(MIXNET_CONTRACT_ARG)
                 .help("Address of the validator contract managing the network")
                 .takes_value(true),
        )
        .arg(Arg::with_name(MNEMONIC_ARG)
                 .long(MNEMONIC_ARG)
                 .help("Mnemonic of the network monitor used for rewarding operators")
                 .takes_value(true)
        )
        .arg(
            Arg::with_name(WRITE_CONFIG_ARG)
                .help("specifies whether a config file based on provided arguments should be saved to a file")
                .long(WRITE_CONFIG_ARG)
                .short('w')
        )
        .arg(
            Arg::with_name(REWARDING_MONITOR_THRESHOLD_ARG)
                .help("Specifies the minimum percentage of monitor test run data present in order to distribute rewards for given interval.")
                .takes_value(true)
                .long(REWARDING_MONITOR_THRESHOLD_ARG)
        )
        .arg(
            Arg::with_name(MIN_MIXNODE_RELIABILITY_ARG)
                .long(MIN_MIXNODE_RELIABILITY_ARG)
                .help("Mixnodes with relialability lower the this get blacklisted by network monitor, get no traffic and cannot be selected into a rewarded set.")
                .takes_value(true)
        )
        .arg(
            Arg::with_name(MIN_GATEWAY_RELIABILITY_ARG)
                .long(MIN_GATEWAY_RELIABILITY_ARG)
                .help("Gateways with relialability lower the this get blacklisted by network monitor, get no traffic and cannot be selected into a rewarded set.")
                .takes_value(true)
        )
        .arg(
            Arg::with_name(ENABLED_CREDENTIALS_MODE_ARG_NAME)
                .long(ENABLED_CREDENTIALS_MODE_ARG_NAME)
                .help("Set this validator api to work in a enabled credentials that would attempt to use gateway with the bandwidth credential requirement")
        );

    #[cfg(feature = "coconut")]
    let base_app = base_app
        .arg(
            Arg::with_name(KEYPAIR_ARG)
                .help("Path to the secret key file")
                .takes_value(true)
                .long(KEYPAIR_ARG),
        )
        .arg(
            Arg::with_name(API_VALIDATORS_ARG)
                .help("specifies list of all validators on the network issuing coconut credentials. Ensure they are properly ordered")
                .long(API_VALIDATORS_ARG)
                .takes_value(true)
        )
        .arg(
            Arg::with_name(COCONUT_ENABLED)
                .help("Flag to indicate whether coconut signer authority is enabled on this API")
                .requires_all(&[KEYPAIR_ARG, MNEMONIC_ARG, API_VALIDATORS_ARG])
                .long(COCONUT_ENABLED),
        );
    base_app.get_matches()
}

async fn wait_for_interrupt(mut shutdown: ShutdownNotifier) {
    wait_for_signal().await;

    log::info!("Sending shutdown");
    shutdown.signal_shutdown().ok();

    log::info!("Waiting for tasks to finish... (Press ctrl-c to force)");
    shutdown.wait_for_shutdown().await;

    log::info!("Stopping nym validator API");
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).expect("Failed to setup SIGTERM channel");
    let mut sigquit = signal(SignalKind::quit()).expect("Failed to setup SIGQUIT channel");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            log::info!("Received SIGINT");
        },
        _ = sigterm.recv() => {
            log::info!("Received SIGTERM");
        }
        _ = sigquit.recv() => {
            log::info!("Received SIGQUIT");
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            log::info!("Received SIGINT");
        },
    }
}

fn setup_logging() {
    let mut log_builder = pretty_env_logger::formatted_timed_builder();
    if let Ok(s) = ::std::env::var("RUST_LOG") {
        log_builder.parse_filters(&s);
    } else {
        // default to 'Info'
        log_builder.filter(None, log::LevelFilter::Info);
    }

    log_builder
        .filter_module("hyper", log::LevelFilter::Warn)
        .filter_module("tokio_reactor", log::LevelFilter::Warn)
        .filter_module("reqwest", log::LevelFilter::Warn)
        .filter_module("mio", log::LevelFilter::Warn)
        .filter_module("want", log::LevelFilter::Warn)
        .filter_module("sled", log::LevelFilter::Warn)
        .filter_module("tungstenite", log::LevelFilter::Warn)
        .filter_module("tokio_tungstenite", log::LevelFilter::Warn)
        .init();
}

fn override_config(mut config: Config, matches: &ArgMatches) -> Config {
    if let Some(id) = matches.value_of(ID) {
        fs::create_dir_all(Config::default_config_directory(Some(id)))
            .expect("Could not create config directory");
        fs::create_dir_all(Config::default_data_directory(Some(id)))
            .expect("Could not create data directory");
        config = config.with_id(id);
    }

    if matches.is_present(MONITORING_ENABLED) {
        config = config.with_network_monitor_enabled(true)
    }

    if matches.is_present(REWARDING_ENABLED) {
        config = config.with_rewarding_enabled(true)
    }

    #[cfg(feature = "coconut")]
    if matches.is_present(COCONUT_ENABLED) {
        config = config.with_coconut_signer_enabled(true)
    }

    #[cfg(feature = "coconut")]
    if let Some(raw_validators) = matches.value_of(API_VALIDATORS_ARG) {
        config = config.with_custom_validator_apis(::config::parse_validators(raw_validators));
    } else if std::env::var(CONFIGURED).is_ok() {
        if let Some(raw_validators) = read_var_if_not_default(API_VALIDATOR) {
            config = config.with_custom_validator_apis(::config::parse_validators(&raw_validators))
        }
    }

    if let Some(raw_validator) = matches.value_of(NYMD_VALIDATOR_ARG) {
        let parsed = match raw_validator.parse() {
            Err(err) => {
                error!("Passed validator argument is invalid - {}", err);
                process::exit(1)
            }
            Ok(url) => url,
        };
        config = config.with_custom_nymd_validator(parsed);
    }

    if let Some(mixnet_contract) = matches.value_of(MIXNET_CONTRACT_ARG) {
        config = config.with_custom_mixnet_contract(mixnet_contract)
    } else if std::env::var(CONFIGURED).is_ok() {
        if let Some(mixnet_contract) = read_var_if_not_default(MIXNET_CONTRACT_ADDRESS) {
            config = config.with_custom_mixnet_contract(mixnet_contract)
        }
    }

    if let Some(mnemonic) = matches.value_of(MNEMONIC_ARG) {
        config = config.with_mnemonic(mnemonic)
    }

    if let Some(monitor_threshold) = matches
        .value_of(REWARDING_MONITOR_THRESHOLD_ARG)
        .map(|t| t.parse::<u8>())
    {
        let monitor_threshold =
            monitor_threshold.expect("Provided monitor threshold is not a number!");
        assert!(
            monitor_threshold <= 100,
            "Provided monitor threshold is greater than 100!"
        );
        config = config.with_minimum_interval_monitor_threshold(monitor_threshold)
    }

    if let Some(reliability) = matches
        .value_of(MIN_MIXNODE_RELIABILITY_ARG)
        .map(|t| t.parse::<u8>())
    {
        config = config.with_min_mixnode_reliability(
            reliability.expect("Provided reliability is not a u8 number!"),
        )
    }

    if let Some(reliability) = matches
        .value_of(MIN_GATEWAY_RELIABILITY_ARG)
        .map(|t| t.parse::<u8>())
    {
        config = config.with_min_gateway_reliability(
            reliability.expect("Provided reliability is not a u8 number!"),
        )
    }

    #[cfg(feature = "coconut")]
    if let Some(keypair_path) = matches.value_of(KEYPAIR_ARG) {
        config = config.with_keypair_path(keypair_path.into())
    }

    if matches.is_present(ENABLED_CREDENTIALS_MODE_ARG_NAME) {
        config = config.with_disabled_credentials_mode(false)
    }

    if matches.is_present(WRITE_CONFIG_ARG) {
        info!("Saving the configuration to a file");
        if let Err(err) = config.save_to_file(None) {
            error!("Failed to write config to a file - {}", err);
            process::exit(1)
        }
    }

    config
}

fn setup_cors() -> Result<Cors> {
    let allowed_origins = AllowedOrigins::all();

    // You can also deserialize this
    let cors = rocket_cors::CorsOptions {
        allowed_origins,
        allowed_methods: vec![Method::Post, Method::Get]
            .into_iter()
            .map(From::from)
            .collect(),
        allowed_headers: AllowedHeaders::all(),
        allow_credentials: true,
        ..Default::default()
    }
    .to_cors()?;

    Ok(cors)
}

fn setup_liftoff_notify(notify: Arc<Notify>) -> AdHoc {
    AdHoc::on_liftoff("Liftoff notifier", |_| {
        Box::pin(async move { notify.notify_one() })
    })
}

fn setup_network_monitor<'a>(
    config: &'a Config,
    system_version: &str,
    rocket: &Rocket<Ignite>,
) -> Option<NetworkMonitorBuilder<'a>> {
    if !config.get_network_monitor_enabled() {
        return None;
    }

    // get instances of managed states
    let node_status_storage = rocket.state::<ValidatorApiStorage>().unwrap().clone();
    let validator_cache = rocket.state::<ValidatorCache>().unwrap().clone();

    Some(NetworkMonitorBuilder::new(
        config,
        system_version,
        node_status_storage,
        validator_cache,
    ))
}

// TODO: Remove if still unused
#[allow(dead_code)]
fn expected_monitor_test_runs(config: &Config, interval_length: Duration) -> usize {
    let test_delay = config.get_network_monitor_run_interval();

    // this is just a rough estimate. In real world there will be slightly fewer test runs
    // as they are not instantaneous and hence do not happen exactly every test_delay
    (interval_length.as_secs() / test_delay.as_secs()) as usize
}

async fn setup_rocket(
    config: &Config,
    _mix_denom: String,
    liftoff_notify: Arc<Notify>,
    _nymd_client: Client<SigningNymdClient>,
) -> Result<Rocket<Ignite>> {
    let openapi_settings = rocket_okapi::settings::OpenApiSettings::default();
    let mut rocket = rocket::build();

    let custom_route_spec = (vec![], custom_openapi_spec());

    mount_endpoints_and_merged_docs! {
        rocket,
        "/v1".to_owned(),
        openapi_settings,
        "/" => custom_route_spec,
        "" => contract_cache::validator_cache_routes(&openapi_settings),
        "/status" => node_status_api::node_status_routes(&openapi_settings, config.get_network_monitor_enabled()),
    }

    let rocket = rocket
        .mount("/swagger", make_swagger_ui(&swagger::get_docs()))
        .attach(setup_cors()?)
        .attach(setup_liftoff_notify(liftoff_notify))
        .attach(ValidatorCache::stage())
        .attach(NodeStatusCache::stage());

    // This is not a very nice approach. A lazy value would be more suitable, but that's still
    // a nightly feature: https://github.com/rust-lang/rust/issues/74465
    let storage = if cfg!(feature = "coconut") || config.get_network_monitor_enabled() {
        Some(ValidatorApiStorage::init(config.get_node_status_api_database_path()).await?)
    } else {
        None
    };

    #[cfg(feature = "coconut")]
    let rocket = if config.get_coconut_signer_enabled() {
        let keypair_bs58 = fs::read_to_string(config.keypair_path())?
            .trim()
            .to_string();
        let keypair = KeyPair::try_from_bs58(keypair_bs58)?;
        rocket.attach(InternalSignRequest::stage(
            _nymd_client,
            _mix_denom,
            keypair,
            QueryCommunicationChannel::new(config.get_all_validator_api_endpoints()),
            storage.clone().unwrap(),
        ))
    } else {
        rocket
    };

    // see if we should start up network monitor
    let rocket = if config.get_network_monitor_enabled() {
        rocket.attach(storage::ValidatorApiStorage::stage(storage.unwrap()))
    } else {
        rocket
    };

    Ok(rocket.ignite().await?)
}

fn custom_openapi_spec() -> OpenApi {
    use rocket_okapi::okapi::openapi3::*;
    OpenApi {
        openapi: OpenApi::default_version(),
        info: Info {
            title: "Validator API".to_owned(),
            description: None,
            terms_of_service: None,
            contact: None,
            license: None,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            ..Default::default()
        },
        servers: get_servers(),
        ..Default::default()
    }
}

fn get_servers() -> Vec<rocket_okapi::okapi::openapi3::Server> {
    if std::env::var_os("CARGO").is_some() {
        return vec![];
    }
    vec![rocket_okapi::okapi::openapi3::Server {
        url: std::env::var("OPEN_API_BASE").unwrap_or_else(|_| "/api/v1/".to_owned()),
        description: Some("API".to_owned()),
        ..Default::default()
    }]
}

async fn run_validator_api(matches: ArgMatches) -> Result<()> {
    let system_version = env!("CARGO_PKG_VERSION");

    // try to load config from the file, if it doesn't exist, use default values
    let id = matches.value_of(ID);
    let config = match Config::load_from_file(id) {
        Ok(cfg) => cfg,
        Err(_) => {
            let config_path = Config::default_config_file_path(id)
                .into_os_string()
                .into_string()
                .unwrap();
            warn!(
                "Could not load the configuration file from {}. Either the file did not exist or was malformed. Using the default values instead",
                config_path
            );
            Config::new()
        }
    };

    let config = override_config(config, &matches);
    // if we just wanted to write data to the config, exit
    if matches.is_present(WRITE_CONFIG_ARG) {
        return Ok(());
    }
    let mix_denom = std::env::var(MIX_DENOM).expect("mix denom not set");

    let signing_nymd_client = Client::new_signing(&config);

    let liftoff_notify = Arc::new(Notify::new());
    // We need a bigger timeout
    let shutdown = ShutdownNotifier::new(10);

    // let's build our rocket!
    let rocket = setup_rocket(
        &config,
        mix_denom,
        Arc::clone(&liftoff_notify),
        signing_nymd_client.clone(),
    )
    .await?;
    let monitor_builder = setup_network_monitor(&config, system_version, &rocket);

    let validator_cache = rocket.state::<ValidatorCache>().unwrap().clone();
    let node_status_cache = rocket.state::<NodeStatusCache>().unwrap().clone();

    // if network monitor is disabled, we're not going to be sending any rewarding hence
    // we're not starting signing client
    let validator_cache_listener = if config.get_network_monitor_enabled() {
        // Main storage
        let storage = rocket.state::<ValidatorApiStorage>().unwrap().clone();

        // setup our daily uptime updater. Note that if network monitor is disabled, then we have
        // no data for the updates and hence we don't need to start it up
        let uptime_updater = HistoricalUptimeUpdater::new(storage.clone());
        let shutdown_listener = shutdown.subscribe();
        tokio::spawn(async move { uptime_updater.run(shutdown_listener).await });

        // spawn the validator cache refresher
        let validator_cache_refresher = ValidatorCacheRefresher::new(
            signing_nymd_client.clone(),
            config.get_caching_interval(),
            validator_cache.clone(),
            Some(storage.clone()),
        );
        let validator_cache_listener = validator_cache_refresher.subscribe();
        let shutdown_listener = shutdown.subscribe();
        tokio::spawn(async move { validator_cache_refresher.run(shutdown_listener).await });

        // spawn rewarded set updater
        let mut rewarded_set_updater =
            RewardedSetUpdater::new(signing_nymd_client, validator_cache.clone(), storage).await?;
        let shutdown_listener = shutdown.subscribe();
        tokio::spawn(async move { rewarded_set_updater.run(shutdown_listener).await.unwrap() });

        validator_cache_listener
    } else {
        // Spawn the validator cache refresher.
        // When the network monitor is not enabled, we spawn the validator cache refresher task
        // with just a nymd client, in contrast to a signing client.
        let nymd_client = Client::new_query(&config);
        let validator_cache_refresher = ValidatorCacheRefresher::new(
            nymd_client,
            config.get_caching_interval(),
            validator_cache.clone(),
            None,
        );
        let validator_cache_listener = validator_cache_refresher.subscribe();
        let shutdown_listener = shutdown.subscribe();
        tokio::spawn(async move { validator_cache_refresher.run(shutdown_listener).await });

        validator_cache_listener
    };

    // Spawn the node status cache refresher.
    // It is primarily refreshed in-sync with the validator cache, however provide a fallback
    // caching interval that is twice the validator cache
    let mut validator_api_cache_refresher = node_status_api::NodeStatusCacheRefresher::new(
        node_status_cache,
        validator_cache,
        validator_cache_listener,
        config.get_caching_interval().saturating_mul(2),
    );
    let shutdown_listener = shutdown.subscribe();
    tokio::spawn(async move { validator_api_cache_refresher.run(shutdown_listener).await });

    // launch the rocket!
    // Rocket handles shutdown on it's own, but its shutdown handling should be incorporated
    // with that of the rest of the tasks.
    // Currently it's runtime is forcefully terminated once the validator-api exits.
    let shutdown_handle = rocket.shutdown();
    tokio::spawn(rocket.launch());

    // to finish building our monitor, we need to have rocket up and running so that we could
    // obtain our bandwidth credential
    if let Some(monitor_builder) = monitor_builder {
        info!("Starting network monitor...");
        // wait for rocket's liftoff stage
        liftoff_notify.notified().await;

        // we're ready to go! spawn the network monitor!
        let runnables = monitor_builder.build().await;
        runnables.spawn_tasks(&shutdown);
    } else {
        info!("Network monitoring is disabled.");
    }

    wait_for_interrupt(shutdown).await;
    shutdown_handle.notify();

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("Starting validator api...");

    cfg_if::cfg_if! {if #[cfg(feature = "console-subscriber")] {
        // instriment tokio console subscriber needs RUSTFLAGS="--cfg tokio_unstable" at build time
        console_subscriber::init();
    }}

    setup_logging();
    let args = parse_args();
    let config_env_file = args
        .value_of(CONFIG_ENV_FILE)
        .map(|s| PathBuf::from_str(s).expect("invalid env config file"));
    setup_env(config_env_file);
    run_validator_api(args).await
}
