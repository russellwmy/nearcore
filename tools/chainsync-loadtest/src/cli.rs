use clap::{AppSettings, Clap};
use futures::future::FutureExt;
use near_chain_configs::GenesisValidationMode;
use std::path::Path;
use std::{env, io};
use tracing::metadata::LevelFilter;
use tracing::{info,error};
use tracing_subscriber::EnvFilter;

#[derive(Clap, Debug)]
#[clap(setting = AppSettings::SubcommandRequiredElseHelp)]
pub(super) struct Cmd {
    #[clap(long)]
    pub chain_id : String,
    /// Sets verbose logging for the given target, or for all targets
    /// if "debug" is given.
    #[clap(long, name = "target")]
    verbose: Option<String>,
    /// Directory for config and data.
    /// Skips consistency checks of the 'genesis.json' file upon startup.
    /// Let's you start `neard` slightly faster.
    #[clap(long)]
    pub unsafe_fast_startup: bool,
}

impl Cmd {
    pub(super) fn parse_and_run() {
        let cmd = Self::parse();
        init_logging(cmd.verbose.as_deref());
        
		let home_dir = Path::new("/tmp");
        let genesis_validation = if cmd.unsafe_fast_startup {
            GenesisValidationMode::UnsafeFast
        } else {
            GenesisValidationMode::Full
        };
		if let Err(e) = nearcore::init_configs(
            home_dir,
            Some(&cmd.chain_id),
            /*account_id = */ None,
            /*test_seed = */ None,
            /*num_shards = <unused>*/1,
            /*fast = <unused>*/false,
			/*genesis =*/ None,
			/*should_download_genesis=*/true,
			/*download_genesis_url=*/None,
            /*should_download_config=*/true,
            /*download_config_url=*/None,
            /*boot_nodes*/None,
            /*max_gas_burnt_view=*/None,
        ) {
            error!("Failed to initialize configs: {:#}", e);
        }

        // Load configs from home.
        let mut near_config = nearcore::config::load_config(home_dir, genesis_validation);

        // Set current version in client config.
        near_config.client_config.version = super::NEARD_VERSION.clone();
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
