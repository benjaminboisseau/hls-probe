use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use url::Url;

pub struct Fetched {
    pub bytes: Vec<u8>,
    pub elapsed: Duration,
    pub final_url: Url,
}

/// Parse curl-style "Name: value" strings into a header map. A user-supplied
/// User-Agent replaces the default one.
pub fn parse_headers(extra: &[String]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    map.insert(
        USER_AGENT,
        HeaderValue::from_static(concat!("hls-probe/", env!("CARGO_PKG_VERSION"))),
    );
    for raw in extra {
        let Some((name, value)) = raw.split_once(':') else {
            bail!("invalid header '{raw}': expected 'Name: value'");
        };
        let name: HeaderName = name
            .trim()
            .parse()
            .with_context(|| format!("invalid header name in '{raw}'"))?;
        let value: HeaderValue = value
            .trim()
            .parse()
            .with_context(|| format!("invalid header value in '{raw}'"))?;
        map.insert(name, value);
    }
    Ok(map)
}

/// `timeout_secs` bounds a whole request, body included: a large segment on
/// a slow link needs more than a playlist does, hence the flag.
pub fn client(extra_headers: &[String], timeout_secs: u64) -> Result<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(timeout_secs.max(1)))
        .default_headers(parse_headers(extra_headers)?)
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

/// Open (and pool) a connection to `url`'s host before anything is timed
/// against it. Segments often live on another host than the playlist; without
/// this the first timed request would carry DNS + TCP + TLS and read as a
/// slow origin. A HEAD on `/` touches no segment, so it warms no cache.
pub fn warm_connection(client: &Client, url: &Url, already: &mut Vec<String>) {
    let Some(host) = url.host_str() else { return };
    let key = format!("{}://{}:{}", url.scheme(), host, url.port_or_known_default().unwrap_or(0));
    if already.contains(&key) {
        return;
    }
    if let Ok(root) = url.join("/") {
        let _ = client.head(root.as_str()).send();
    }
    already.push(key);
}

/// Resolve a possibly relative playlist URI against the URL it was found in.
pub fn resolve(base: &Url, uri: &str) -> Result<Url> {
    base.join(uri)
        .with_context(|| format!("resolving '{uri}' against {base}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_curl_style_headers() {
        let map = parse_headers(&[
            "Authorization: Bearer abc123".to_string(),
            "x-custom:  spaced value ".to_string(),
        ])
        .unwrap();
        assert_eq!(map["authorization"], "Bearer abc123");
        assert_eq!(map["x-custom"], "spaced value");
    }

    #[test]
    fn default_user_agent_present_and_overridable() {
        let map = parse_headers(&[]).unwrap();
        assert!(map["user-agent"].to_str().unwrap().starts_with("hls-probe/"));

        let map = parse_headers(&["User-Agent: AppleCoreMedia/1.0".to_string()]).unwrap();
        assert_eq!(map["user-agent"], "AppleCoreMedia/1.0");
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn rejects_header_without_colon() {
        assert!(parse_headers(&["not-a-header".to_string()]).is_err());
    }

    #[test]
    fn value_may_contain_colons() {
        let map = parse_headers(&["X-Time: 12:34:56".to_string()]).unwrap();
        assert_eq!(map["x-time"], "12:34:56");
    }
}
