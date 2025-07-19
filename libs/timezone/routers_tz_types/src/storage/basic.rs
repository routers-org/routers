use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::timezone::Timezone;

#[derive(Encode, Decode, Serialize, Deserialize, Debug, Clone)]
pub struct BasicStorageBackend {
    #[bincode(with_serde)]
    pub polygons: Vec<Timezone>,
}