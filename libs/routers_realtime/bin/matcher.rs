use log::info;
use std::thread::sleep;
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    info!("matcher started");

    loop {
        sleep(Duration::from_secs(1));
    }
}
