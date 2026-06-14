use log::info;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    info!("matcher started");

    loop {}

    Ok(())
}
