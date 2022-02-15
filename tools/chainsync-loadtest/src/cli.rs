use super::DEFAULT_HOME;
use clap::{AppSettings, Clap};
use futures::future::FutureExt;
use near_chain_configs::GenesisValidationMode;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::{env, io};
use tracing::metadata::LevelFilter;
use tracing::{info};
use tracing_subscriber::EnvFilter;

#[derive(Clap, Debug)]
#[clap(setting = AppSettings::SubcommandRequiredElseHelp)]
pub(super) struct Cmd {
    /// Sets verbose logging for the given target, or for all targets
    /// if "debug" is given.
    #[clap(long, name = "target")]
    verbose: Option<String>,
    /// Directory for config and data.
    #[clap(long, parse(from_os_str), default_value_os = DEFAULT_HOME.as_os_str())]
    home: PathBuf,
    /// Skips consistency checks of the 'genesis.json' file upon startup.
    /// Let's you start `neard` slightly faster.
    #[clap(long)]
    pub unsafe_fast_startup: bool,

    /// Set the boot nodes to bootstrap network from.
    #[clap(long)]
    boot_nodes: Option<String>,
    /// Customize network listening address (useful for running multiple nodes on the same machine).
    #[clap(long)]
    network_addr: Option<SocketAddr>,
}

impl Cmd {
    pub(super) fn parse_and_run() {
        let cmd = Self::parse();
        init_logging(cmd.verbose.as_deref());
        let home_dir = &cmd.home;
        let genesis_validation = if cmd.unsafe_fast_startup {
            GenesisValidationMode::UnsafeFast
        } else {
            GenesisValidationMode::Full
        };

        // Load configs from home.
        let mut near_config = crate::nearcore::config::load_config(home_dir, genesis_validation);

        // Set current version in client config.
        near_config.client_config.version = super::NEARD_VERSION.clone();
        if let Some(boot_nodes) = cmd.boot_nodes {
            if !boot_nodes.is_empty() {
                near_config.network_config.boot_nodes = boot_nodes
                    .split(',')
                    .map(|chunk| chunk.parse().expect("Failed to parse PeerInfo"))
                    .collect();
            }
        }
        if let Some(network_addr) = cmd.network_addr {
            near_config.network_config.addr = Some(network_addr);
        }
        let sys = actix::System::new();
        sys.block_on(async move {
            crate::nearcore::start_with_config(home_dir, near_config).expect("start_with_config");

            let sig = if cfg!(unix) {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigint = signal(SignalKind::interrupt()).unwrap();
                let mut sigterm = signal(SignalKind::terminate()).unwrap();
                futures::select! {
                    _ = sigint .recv().fuse() => "SIGINT",
                    _ = sigterm.recv().fuse() => "SIGTERM"
                }
            } else {
                tokio::signal::ctrl_c().await.unwrap();
                "Ctrl+C"
            };
            info!(target: "neard", "Got {}, stopping...", sig);
            actix::System::current().stop();
        });
        sys.run().unwrap();
    }
}

fn init_logging(verbose: Option<&str>) {
    const DEFAULT_RUST_LOG: &'static str =
        "tokio_reactor=info,near=info,stats=info,telemetry=info,\
         delay_detector=info,near-performance-metrics=info,\
         near-rust-allocator-proxy=info,warn";

    let rust_log = env::var("RUST_LOG");
    let rust_log = rust_log.as_ref().map(String::as_str).unwrap_or(DEFAULT_RUST_LOG);
    let mut env_filter = EnvFilter::new(rust_log);

    if let Some(module) = verbose {
        env_filter = env_filter
            .add_directive("cranelift_codegen=warn".parse().unwrap())
            .add_directive("cranelift_codegen=warn".parse().unwrap())
            .add_directive("h2=warn".parse().unwrap())
            .add_directive("trust_dns_resolver=warn".parse().unwrap())
            .add_directive("trust_dns_proto=warn".parse().unwrap());

        if module.is_empty() {
            env_filter = env_filter.add_directive(LevelFilter::DEBUG.into());
        } else {
            env_filter = env_filter.add_directive(format!("{}=debug", module).parse().unwrap());
        }
    }

    tracing_subscriber::fmt::Subscriber::builder()
        .with_span_events(
            tracing_subscriber::fmt::format::FmtSpan::ENTER
                | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
        )
        .with_env_filter(env_filter)
        .with_writer(io::stderr)
        .init();
}
