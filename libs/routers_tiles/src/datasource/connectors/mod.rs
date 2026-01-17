pub mod repositories {
    pub mod big_table {
        use bigtable_rs::bigtable::BigTableConnection;
        use core::time::Duration;

        pub(crate) const READ_ONLY: bool = true;
        pub(crate) const CHANNEL_SIZE: usize = 4;
        pub(crate) const TIMEOUT: Option<Duration> = Some(Duration::from_secs(20));

        pub struct BigTableRepository {
            pub connection: BigTableConnection,
            pub table_name: String,
        }
    }
}

pub mod bigtable;
