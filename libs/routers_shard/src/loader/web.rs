//! Browser-side [`ShardFetcher`] using `window.fetch`.
//!
//! Avoids pulling in `reqwest` (which would balloon the wasm bundle) by
//! talking directly to `web-sys`. Only built on `wasm32`.

use js_sys::Uint8Array;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, Response};

use super::fetcher::ShardFetcher;

#[derive(Debug, Clone)]
pub struct WebShardFetcher {
    base_url: String,
}

impl WebShardFetcher {
    /// Fetch keys relative to `base_url`. A trailing slash on `base_url`
    /// is added if missing so callers can pass either form.
    pub fn new(base_url: impl Into<String>) -> Self {
        let mut url = base_url.into();
        if !url.ends_with('/') {
            url.push('/');
        }
        Self { base_url: url }
    }
}

#[derive(Debug)]
pub enum WebFetchError {
    NoWindow,
    Request(String),
    Status(u16),
    Body(String),
}

impl core::fmt::Display for WebFetchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WebFetchError::NoWindow => write!(f, "no `window` global available"),
            WebFetchError::Request(e) => write!(f, "request failed: {e}"),
            WebFetchError::Status(code) => write!(f, "HTTP {code}"),
            WebFetchError::Body(e) => write!(f, "failed to read body: {e}"),
        }
    }
}

fn js_value_to_string(v: &JsValue) -> String {
    v.as_string()
        .or_else(|| js_sys::JSON::stringify(v).ok().and_then(|s| s.as_string()))
        .unwrap_or_else(|| "<unprintable JsValue>".to_string())
}

impl ShardFetcher for WebShardFetcher {
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
