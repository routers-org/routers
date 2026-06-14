use log::info;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    info!("historian started");

    loop {}

    Ok(())
}
