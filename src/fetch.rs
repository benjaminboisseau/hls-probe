use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use url::Url;

pub struct Fetched {
    pub bytes: Vec<u8>,
    pub elapsed: Duration,
    pub final_url: Url,
}

pub fn client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("hls-probe/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")
}

pub fn fetch(client: &Client, url: &Url) -> Result<Fetched> {
    let start = Instant::now();
    let resp = client
        .get(url.as_str())
        .send()
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;
    let final_url = Url::parse(resp.url().as_str())?;
    let bytes = resp.bytes()?.to_vec();
    Ok(Fetched {
        bytes,
        elapsed: start.elapsed(),
        final_url,
    })
}

/// Resolve a possibly relative playlist URI against the URL it was found in.
pub fn resolve(base: &Url, uri: &str) -> Result<Url> {
    base.join(uri)
        .with_context(|| format!("resolving '{uri}' against {base}"))
}
