use routers_tz_types::storage::basic::BasicStorageBackend;
use routers_tz_types::timezone::internal::TimezoneBuild;

use crate::BoxError;
use crate::codegen::Backend;

pub fn build(timezones: &[TimezoneBuild]) -> Result<(), BoxError> {
    let (names, geometries) = timezones
        .iter()
        .map(|tz| (tz.name.clone(), tz.geometry.clone()))
        .unzip();

    Backend {
        module: "basic",
        type_name: "BasicStorageBackend",
    }
    .emit(BasicStorageBackend { geometries, names })
}
