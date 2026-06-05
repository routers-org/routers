//! Browser-side [`Fetcher`] using `window.fetch`.

use js_sys::Uint8Array;
use thiserror::Error;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, Response};

use super::fetcher::ShardFetcher;

#[derive(Debug, Clone)]
pub struct WebFetcher {
    base_url: String,
}

impl WebFetcher {
    /// Fetches shards from the given base URL.
    pub fn new(base_url: impl Into<String>) -> Self {
        let mut url = base_url.into();
        if !url.ends_with('/') {
            url.push('/');
        }

        Self { base_url: url }
    }
}

#[derive(Error, Debug)]
pub enum WebFetchError {
    #[error("no `window` global variable is available")]
    NoWindow,
    #[error("request failed: {0}")]
    Request(String),
    #[error("failed with http code: {0}")]
    Status(u16),
    #[error("failed to read body: {0}")]
    Body(String),
}

fn js_value_to_string(v: &JsValue) -> String {
    v.as_string()
        .or_else(|| js_sys::JSON::stringify(v).ok().and_then(|s| s.as_string()))
        .unwrap_or_else(|| "<unprintable JsValue>".to_string())
}

impl ShardFetcher for WebFetcher {
    type Error = WebFetchError;

    async fn fetch(&self, key: &str) -> Result<Vec<u8>, Self::Error> {
        let url = format!("{}{}", self.base_url, key);

        let init = RequestInit::new();
        let request = Request::new_with_str_and_init(&url, &init)
            .map_err(|e| WebFetchError::Request(js_value_to_string(&e)))?;

        let window = web_sys::window().ok_or(WebFetchError::NoWindow)?;
        let resp_value = JsFuture::from(window.fetch_with_request(&request))
            .await
            .map_err(|e| WebFetchError::Request(js_value_to_string(&e)))?;

        let response: Response = resp_value
            .dyn_into()
            .map_err(|e| WebFetchError::Request(js_value_to_string(&e)))?;

        if !response.ok() {
            return Err(WebFetchError::Status(response.status()));
        }

        let buf_promise = response
            .array_buffer()
            .map_err(|e| WebFetchError::Body(js_value_to_string(&e)))?;
        let buf_value = JsFuture::from(buf_promise)
            .await
            .map_err(|e| WebFetchError::Body(js_value_to_string(&e)))?;
        let array = Uint8Array::new(&buf_value);
        Ok(array.to_vec())
    }
}
