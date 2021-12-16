// Copyright 2020 - Nym Technologies SA <contact@nymtech.net>
// SPDX-License-Identifier: Apache-2.0

use clap::{crate_version, App, ArgMatches};

mod commands;
mod config;
mod node;

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    setup_logging();
    println!("{}", banner());

    let arg_matches = App::new("Nym Mixnet Gateway")
        .version(crate_version!())
        .long_version(&*long_version())
        .author("Nymtech")
        .about("Implementation of the Nym Mixnet Gateway")
        .subcommand(commands::init::command_args())
        .subcommand(commands::run::command_args())
        .subcommand(commands::sign::command_args())
        .subcommand(commands::upgrade::command_args())
        .get_matches();

    execute(arg_matches).await;
}

async fn execute(matches: ArgMatches<'static>) {
    match matches.subcommand() {
        ("init", Some(m)) => commands::init::execute(m.clone()).await,
        ("run", Some(m)) => commands::run::execute(m.clone()).await,
        ("upgrade", Some(m)) => commands::upgrade::execute(m.clone()).await,
        ("sign", Some(m)) => commands::sign::execute(m),
        _ => println!("{}", usage()),
    }
}

fn usage() -> &'static str {
    "usage: --help to see available options.\n\n"
}

fn banner() -> String {
    format!(
        r#"

      _ __  _   _ _ __ ___
     | '_ \| | | | '_ \ _ \
     | | | | |_| | | | | | |
     |_| |_|\__, |_| |_| |_|
            |___/

             (gateway - version {:})

    "#,
        crate_version!()
    )
}

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
        env!("VERGEN_CARGO_PROFILE"),
    )
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
