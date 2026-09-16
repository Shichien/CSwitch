mod app;
mod codex_process;
mod config;
mod desktop;
mod gateway;
mod gateway_transform;
mod model_catalog;
mod network;
mod oauth;
mod operation_lock;
mod profiles;
mod progress;
mod provider_sync;
mod sse;
mod tray;
mod upstream;

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    if gateway::run_from_args(std::env::args())? {
        return Ok(());
    }
    desktop::run()
}
