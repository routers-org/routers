use log::info;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    info!("orchestrator started");

    loop {}

    Ok(())
}
