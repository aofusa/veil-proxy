// F-267: veil::api::client の再公開と便利関数（JSON の送受信・再試行）。
//! The HTTP client of veil ([`veil::api::client`]) plus helpers.
//!
//! ```no_run
//! # async fn f() -> Result<(), veil_kit::client::ClientError> {
//! use veil_kit::client::{get_json, retry, Client, Retry};
//! #[derive(serde::Deserialize)]
//! struct Item { id: u64 }
//! let client = Client::new();
//! let items: Vec<Item> = get_json(&client, "https://api.example.com/items").await?;
//! let res = retry(Retry::default(), || client.get("https://api.example.com/health").send()).await?;
//! # let _ = (items, res); Ok(()) }
//! ```

pub use veil::api::client::*;

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::future::Future;
use std::time::Duration;

/// `GET url`, require 2xx, decode JSON.
pub async fn get_json<T: DeserializeOwned>(client: &Client, url: &str) -> Result<T, ClientError> {
    client.get(url).send().await?.error_for_status()?.json()
}

/// `POST url` with a JSON body, require 2xx, decode JSON.
pub async fn post_json<B: Serialize + ?Sized, T: DeserializeOwned>(
    client: &Client,
    url: &str,
    body: &B,
) -> Result<T, ClientError> {
    client
        .post(url)
        .json(body)
        .send()
        .await?
        .error_for_status()?
        .json()
}

/// Retry policy of [`retry`]: attempts and exponential backoff.
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    /// Total attempts (default 3).
    pub attempts: u32,
    /// Delay before the second attempt (default 100 ms); doubled each time.
    pub backoff: Duration,
    /// Upper bound of the delay (default 2 s).
    pub max_backoff: Duration,
    /// Also retry 502, 503 and 504 answers (default `true`).
    pub retry_gateway_errors: bool,
}

impl Default for Retry {
    fn default() -> Self {
        Retry {
            attempts: 3,
            backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
            retry_gateway_errors: true,
        }
    }
}

/// Runs `f` until it succeeds, a non-retryable error occurs, or the attempts are used up. Only use it for
/// requests that are safe to repeat (idempotent).
pub async fn retry<F, Fut>(policy: Retry, mut f: F) -> Result<ClientResponse, ClientError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<ClientResponse, ClientError>>,
{
    let mut delay = policy.backoff;
    let mut attempt = 1;
    loop {
        let r = f().await;
        let again = match &r {
            Ok(res) => policy.retry_gateway_errors && matches!(res.status(), 502..=504),
            Err(e) => e.is_retryable(),
        };
        if !again || attempt >= policy.attempts.max(1) {
            return r;
        }
        veil::runtime::sleep(delay).await;
        delay = (delay * 2).min(policy.max_backoff);
        attempt += 1;
    }
}
