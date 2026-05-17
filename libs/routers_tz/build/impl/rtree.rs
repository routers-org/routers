use routers_tz_types::storage::rtree::EncodableRTreeStorageBackend;
use routers_tz_types::timezone::internal::TimezoneBuild;

use crate::BoxError;
use crate::codegen::Backend;

pub fn build(timezones: &[TimezoneBuild]) -> Result<(), BoxError> {
    Backend { module: "rtree" }.emit(EncodableRTreeStorageBackend::new(timezones))
}
