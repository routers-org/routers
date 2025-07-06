use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use serde::de::DeserializeOwned;
use serde_qs::Config;

// Extractor for `serde_qs` support
pub struct QueryParams<T>(pub T);

impl<B, T> FromRequestParts<B> for QueryParams<T>
where
    B: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, _: &B) -> Result<Self, Self::Rejection> {
        let query = parts
            .uri
            .query()
            .ok_or_else(|| (StatusCode::BAD_REQUEST, "Missing query string".to_string()))?;

        let config = Config::new(5, false);
        let params: T = config.deserialize_str(query).map_err(|err| {
            (
                StatusCode::BAD_REQUEST,
                format!("Failed to deserialize query string: {err}"),
            )
        })?;

        Ok(QueryParams(params))
    }
}
