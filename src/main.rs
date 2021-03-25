use server::server::run_server;
use tokio::join;
use util::process::{setup_process_exit_on_panic, wait_for_sigint};

pub mod accept;
pub mod config;
pub mod database;
pub mod metrics;
pub mod replay;
pub mod server;
pub mod util;
pub mod worker_threads;

#[macro_use]
pub mod error;

use crate::config::InnerSettings;
use tokio_util::sync::CancellationToken;

const VERSION: &'static str = env!("CARGO_PKG_VERSION");

async fn do_run_server() {
    let maybe_config = InnerSettings::from_env();
    let config = match maybe_config {
        Err(e) => {
            log::info!("Failed to load config: {}", e);
            return;
        }
        Ok(o) => o,
    };

    log::info!(
        "Listening on port {}, prometheus server started on port {}.",
        config.server.port,
        config.server.prometheus_port
    );
    if !start_prometheus_server(&config) {
        return;
    }
    let shutdown_token = CancellationToken::new();
    let f1 = run_server(config, shutdown_token.clone());
    let f2 = async {
        wait_for_sigint().await;
        log::debug!("Received a SIGINT, shutting down");
        shutdown_token.cancel();
    };
    join!(f1, f2);
    /* Worker threads are joined once we drop the server. */
}

fn start_prometheus_server(config: &config::Settings) -> bool {
    let addr = format!("127.0.0.1:{}", config.server.prometheus_port);
    let parsed_addr = match addr.parse() {
        Err(e) => {
            log::error!("Failed to parse prometheus port: {}", e);
            return false;
        }
        Ok(a) => a,
    };
    match prometheus_exporter::start(parsed_addr) {
        Ok(..) => true,
        Err(e) => {
            log::debug!("Could not launch prometheus HTTP server: {}", e);
            false
        }
    }
}

pub fn main() {
    env_logger::init();
    log::info!("Server version {}.", VERSION);
    setup_process_exit_on_panic();
    let local_loop = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    local_loop.block_on(do_run_server());
}
