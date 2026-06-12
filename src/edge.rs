use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use m3u8_rs::Playlist;
use reqwest::blocking::Client;
use reqwest::header::HeaderMap;
use serde_json::{json, Value};
use url::Url;

use crate::fetch;

pub struct Sample {
    pub ttfb_ms: f64,
    pub total_ms: f64,
    pub bytes: u64,
    pub cache_status: Option<String>,
    pub age: Option<u64>,
}

pub struct Pair {
    pub uri: String,
    pub fresh: Sample,
    pub warmed: Sample,
}

impl Pair {
    pub fn penalty_ms(&self) -> f64 {
        self.fresh.ttfb_ms - self.warmed.ttfb_ms
    }
}

pub struct EdgeReport {
    pub pairs: Vec<Pair>,
    pub avg_fresh_ttfb: f64,
    pub avg_warmed_ttfb: f64,
    pub avg_penalty_ms: f64,
}

impl EdgeReport {
    /// A one-line reading of the numbers, kept deliberately cautious.
    pub fn verdict(&self) -> &'static str {
        if self.pairs.len() < 2 {
            "too few pairs to conclude; rerun with more"
        } else if self.avg_penalty_ms > 100.0 && self.pairs.iter().all(|p| p.penalty_ms() > 0.0) {
            "consistent fresh-segment penalty: first request travels to the origin \
             (CDN cache miss), the warmed one is served from the edge"
        } else if self.avg_penalty_ms.abs() < 50.0 {
            "no meaningful fresh-segment penalty observed (cache already warm, \
             or origin very close to the edge)"
        } else {
            "mixed results; network jitter may dominate, rerun with more pairs"
        }
    }
}

fn cache_status(headers: &HeaderMap) -> Option<String> {
    // Common CDN cache-decision headers (Akamai with debug Pragma, CloudFront,
    // Cloudflare, Fastly/Varnish flavours).
    for key in ["x-cache", "x-cache-status", "cf-cache-status", "x-cdn-cache-status"] {
        if let Some(s) = headers.get(key).and_then(|v| v.to_str().ok()) {
            return Some(s.to_string());
        }
    }
    None
}

fn age_seconds(headers: &HeaderMap) -> Option<u64> {
    headers.get("age")?.to_str().ok()?.trim().parse().ok()
}

fn sample(client: &Client, url: &Url) -> Result<Sample> {
    let start = Instant::now();
    let resp = client
        .get(url.as_str())
        // Ask Akamai to reveal its cache decision; other CDNs ignore this.
        .header("Pragma", "akamai-x-cache-on")
        .send()?
        .error_for_status()?;
    let ttfb = start.elapsed();
    let headers = resp.headers().clone();
    let body = resp.bytes()?;
    let total = start.elapsed();
    Ok(Sample {
        ttfb_ms: ttfb.as_secs_f64() * 1000.0,
        total_ms: total.as_secs_f64() * 1000.0,
        bytes: body.len() as u64,
        cache_status: cache_status(&headers),
        age: age_seconds(&headers),
    })
}

/// Measure the CDN cache-miss penalty at the live edge.
///
/// Watches the playlist until a segment is published, requests it immediately
/// ("fresh": on a cold edge this request must travel to the origin), then
/// requests the very same URL one target duration later ("warmed": the edge
/// just cached it for us). The TTFB difference, repeated over `wanted` pairs,
/// is the penalty the first viewer per edge node pays on every new segment.
pub fn edge_test(client: &Client, url: &Url, wanted: usize, quiet: bool) -> Result<EdgeReport> {
    let mut pairs: Vec<Pair> = Vec::new();
    let mut last_edge_seq: Option<u64> = None;
    let mut deadline: Option<Instant> = None;

    loop {
        let fetched = fetch::fetch(client, url)?;
        let pl = match m3u8_rs::parse_playlist_res(&fetched.bytes) {
            Ok(Playlist::MediaPlaylist(p)) => p,
            Ok(Playlist::MasterPlaylist(_)) => bail!("--edge-test needs a media playlist"),
            Err(e) => bail!("playlist no longer parses: {e}"),
        };
        if pl.end_list {
            bail!("playlist has EXT-X-ENDLIST: not a live stream");
        }
        let target = pl.target_duration.max(1);
        // Give the whole run a generous but bounded budget.
        let deadline = *deadline
            .get_or_insert_with(|| Instant::now() + Duration::from_secs(wanted as u64 * 6 * target + 30));

        let edge_seq = pl.media_sequence + pl.segments.len() as u64;
        if let (Some(prev), Some(newest)) = (last_edge_seq, pl.segments.last()) {
            if edge_seq > prev {
                // This segment appeared within the last poll interval, so it
                // is at most target_duration/2 old: as fresh as it gets.
                let seg_url = fetch::resolve(&fetched.final_url, &newest.uri)?;
                let fresh = sample(client, &seg_url)?;
                sleep(Duration::from_secs(target));
                let warmed = sample(client, &seg_url)?;
                let pair = Pair {
                    uri: newest.uri.clone(),
                    fresh,
                    warmed,
                };
                if !quiet {
                    eprintln!(
                        "pair {:>2}/{wanted}: fresh {:>5.0} ms{}  warmed {:>5.0} ms{}  penalty {:+.0} ms",
                        pairs.len() + 1,
                        pair.fresh.ttfb_ms,
                        cache_label(&pair.fresh),
                        pair.warmed.ttfb_ms,
                        cache_label(&pair.warmed),
                        pair.penalty_ms(),
                    );
                }
                pairs.push(pair);
                if pairs.len() >= wanted {
                    break;
                }
            }
        }
        last_edge_seq = Some(edge_seq);

        if Instant::now() > deadline {
            if pairs.is_empty() {
                bail!("playlist did not advance before the time budget ran out");
            }
            if !quiet {
                eprintln!("time budget exhausted, reporting {} pair(s)", pairs.len());
            }
            break;
        }
        sleep(Duration::from_secs_f64((target as f64 / 2.0).max(1.0)));
    }

    let n = pairs.len() as f64;
    let avg_fresh = pairs.iter().map(|p| p.fresh.ttfb_ms).sum::<f64>() / n;
    let avg_warmed = pairs.iter().map(|p| p.warmed.ttfb_ms).sum::<f64>() / n;
    Ok(EdgeReport {
        avg_fresh_ttfb: avg_fresh,
        avg_warmed_ttfb: avg_warmed,
        avg_penalty_ms: avg_fresh - avg_warmed,
        pairs,
    })
}

pub fn cache_label(s: &Sample) -> String {
    // Keep only the decision token (TCP_MISS, TCP_HIT, ...) for the table;
    // the full header value stays available in the JSON output.
    let short = s
        .cache_status
        .as_deref()
        .map(|c| c.split_whitespace().next().unwrap_or(c));
    match (short, s.age) {
        (Some(c), Some(a)) => format!(" [{c}, age {a}s]"),
        (Some(c), None) => format!(" [{c}]"),
        (None, Some(a)) => format!(" [age {a}s]"),
        (None, None) => String::new(),
    }
}

pub fn report_json(r: &EdgeReport) -> Value {
    json!({
        "pairs": r.pairs.iter().map(|p| json!({
            "uri": p.uri,
            "fresh": sample_json(&p.fresh),
            "warmed": sample_json(&p.warmed),
            "penalty_ms": p.penalty_ms(),
        })).collect::<Vec<_>>(),
        "avg_fresh_ttfb_ms": r.avg_fresh_ttfb,
        "avg_warmed_ttfb_ms": r.avg_warmed_ttfb,
        "avg_penalty_ms": r.avg_penalty_ms,
        "verdict": r.verdict(),
    })
}

fn sample_json(s: &Sample) -> Value {
    json!({
        "ttfb_ms": s.ttfb_ms,
        "total_ms": s.total_ms,
        "bytes": s.bytes,
        "cache_status": s.cache_status,
        "age": s.age,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                k.parse::<HeaderName>().unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn reads_akamai_style_cache_header() {
        let h = headers(&[("x-cache", "TCP_HIT from a23-x-x (AkamaiGHost)"), ("age", "4")]);
        assert_eq!(
            cache_status(&h).as_deref(),
            Some("TCP_HIT from a23-x-x (AkamaiGHost)")
        );
        assert_eq!(age_seconds(&h), Some(4));
    }

    #[test]
    fn missing_headers_yield_none() {
        let h = HeaderMap::new();
        assert_eq!(cache_status(&h), None);
        assert_eq!(age_seconds(&h), None);
    }

    #[test]
    fn verdict_flags_consistent_penalty() {
        let mk = |fresh: f64, warmed: f64| Pair {
            uri: "s.mp4".into(),
            fresh: Sample { ttfb_ms: fresh, total_ms: fresh, bytes: 0, cache_status: None, age: None },
            warmed: Sample { ttfb_ms: warmed, total_ms: warmed, bytes: 0, cache_status: None, age: None },
        };
        let report = EdgeReport {
            pairs: vec![mk(900.0, 120.0), mk(1100.0, 140.0), mk(800.0, 110.0)],
            avg_fresh_ttfb: 933.0,
            avg_warmed_ttfb: 123.0,
            avg_penalty_ms: 810.0,
        };
        assert!(report.verdict().contains("consistent"));
    }
}
