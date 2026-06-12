use std::thread::sleep;
use std::time::Duration;

use anyhow::{bail, Result};
use chrono::Utc;
use m3u8_rs::Playlist;
use reqwest::blocking::Client;
use serde_json::{json, Value};
use url::Url;

use crate::fetch;

pub struct LiveStats {
    pub refreshes: u32,
    pub stale_refreshes: u32,
    pub new_segments: u64,
    pub observed_seconds: f64,
    pub avg_latency: Option<f64>,
    pub min_latency: Option<f64>,
    pub max_latency: Option<f64>,
}

/// Poll a live media playlist and measure how it advances.
///
/// The playlist is fetched every `target_duration / 2` (the reload interval
/// RFC 8216 suggests for clients that have not yet seen a change). A refresh
/// is "stale" when the media sequence and segment count did not move at all;
/// a healthy origin should never be stale for more than two consecutive
/// polls. When EXT-X-PROGRAM-DATE-TIME is present, the distance between now
/// and the end of the newest segment estimates how far the playlist edge
/// lags real time.
pub fn monitor(client: &Client, url: &Url, refreshes: u32, quiet: bool) -> Result<LiveStats> {
    let mut last_seen_sequence: Option<u64> = None;
    let mut stale = 0u32;
    let mut new_segments = 0u64;
    let mut latencies: Vec<f64> = Vec::new();
    let started = std::time::Instant::now();

    for round in 0..refreshes {
        let fetched = fetch::fetch(client, url)?;
        let playlist = match m3u8_rs::parse_playlist_res(&fetched.bytes) {
            Ok(Playlist::MediaPlaylist(p)) => p,
            Ok(Playlist::MasterPlaylist(_)) => bail!("--live needs a media playlist"),
            Err(e) => bail!("refresh {round}: playlist no longer parses: {e}"),
        };
        if playlist.end_list {
            bail!("playlist has EXT-X-ENDLIST: not a live stream");
        }

        let edge_sequence = playlist.media_sequence + playlist.segments.len() as u64;
        match last_seen_sequence {
            None => {}
            Some(prev) if edge_sequence > prev => {
                new_segments += edge_sequence - prev;
                stale = stale.saturating_sub(stale); // reset on progress
            }
            Some(_) => stale += 1,
        }
        last_seen_sequence = Some(edge_sequence);

        let latency = playlist
            .segments
            .iter()
            .rev()
            .find_map(|s| s.program_date_time.map(|pdt| (pdt, s.duration)))
            .map(|(pdt, duration)| {
                let edge = pdt + chrono::Duration::milliseconds((duration * 1000.0) as i64);
                (Utc::now().fixed_offset() - edge).num_milliseconds() as f64 / 1000.0
            });
        if let Some(l) = latency {
            latencies.push(l);
        }

        if !quiet {
            eprintln!(
                "refresh {:>2}/{refreshes}: edge seq {edge_sequence}, {} segments, {}",
                round + 1,
                playlist.segments.len(),
                latency.map_or("no PDT".to_string(), |l| format!("edge lag {l:.1}s")),
            );
        }

        if round + 1 < refreshes {
            sleep(Duration::from_secs_f64(
                (playlist.target_duration as f64 / 2.0).max(1.0),
            ));
        }
    }

    let avg = (!latencies.is_empty())
        .then(|| latencies.iter().sum::<f64>() / latencies.len() as f64);
    let min = latencies.iter().cloned().reduce(f64::min);
    let max = latencies.iter().cloned().reduce(f64::max);

    Ok(LiveStats {
        refreshes,
        stale_refreshes: stale,
        new_segments,
        observed_seconds: started.elapsed().as_secs_f64(),
        avg_latency: avg,
        min_latency: min,
        max_latency: max,
    })
}

pub fn stats_json(s: &LiveStats) -> Value {
    json!({
        "refreshes": s.refreshes,
        "stale_refreshes": s.stale_refreshes,
        "new_segments": s.new_segments,
        "observed_seconds": s.observed_seconds,
        "edge_lag_seconds": {
            "avg": s.avg_latency,
            "min": s.min_latency,
            "max": s.max_latency,
        },
    })
}
