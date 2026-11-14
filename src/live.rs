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
    /// Refreshes that found the playlist unchanged. Polling at half a target
    /// duration, roughly every other refresh is stale on a healthy stream:
    /// the count alone says little, the longest stall is the signal.
    pub stale_refreshes: u32,
    /// Longest run of consecutive unchanged refreshes.
    pub max_consecutive_stale: u32,
    /// Longest time the playlist went without publishing a segment.
    pub longest_stall_seconds: f64,
    pub target_duration: f64,
    /// Times the playlist edge moved backwards (origin or CDN serving an
    /// older copy than one already seen).
    pub regressions: u32,
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
/// is "stale" when the playlist edge did not move; a healthy origin publishes
/// a segment every target duration, so it should not stay stale for more
/// than two consecutive polls. When EXT-X-PROGRAM-DATE-TIME is present, the distance between now
/// and the end of the newest segment estimates how far the playlist edge
/// lags real time.
pub fn monitor(client: &Client, url: &Url, refreshes: u32, quiet: bool) -> Result<LiveStats> {
    let mut last_seen_sequence: Option<u64> = None;
    let mut stale = 0u32;
    let mut stale_run = 0u32;
    let mut max_stale_run = 0u32;
    let mut regressions = 0u32;
    let mut last_advance: Option<std::time::Instant> = None;
    let mut longest_stall = 0f64;
    let mut target_duration = 0f64;
    let mut new_segments = 0u64;
    let mut latencies: Vec<f64> = Vec::new();
    let started = std::time::Instant::now();

    for round in 0..refreshes {
        let fetched = fetch::fetch(client, url)?;
        let playlist = match crate::analyze::parse_playlist(&fetched.bytes) {
            Ok(Playlist::MediaPlaylist(p)) => p,
            Ok(Playlist::MasterPlaylist(_)) => bail!("--live needs a media playlist"),
            Err(e) => bail!("refresh {round}: playlist no longer parses: {e}"),
        };
        if playlist.end_list {
            bail!("playlist has EXT-X-ENDLIST: not a live stream");
        }

        target_duration = playlist.target_duration as f64;
        let edge_sequence = playlist.media_sequence + playlist.segments.len() as u64;
        let now = std::time::Instant::now();
        match last_seen_sequence {
            None => last_advance = Some(now),
            Some(prev) if edge_sequence > prev => {
                new_segments += edge_sequence - prev;
                stale_run = 0;
                last_advance = Some(now);
            }
            Some(prev) => {
                if edge_sequence < prev {
                    regressions += 1;
                }
                stale += 1;
                stale_run += 1;
                max_stale_run = max_stale_run.max(stale_run);
            }
        }
        if let Some(t) = last_advance {
            longest_stall = longest_stall.max(now.duration_since(t).as_secs_f64());
        }
        // Keep the furthest edge seen, so a stale copy is not counted as
        // progress when the fresh one comes back.
        last_seen_sequence = Some(last_seen_sequence.map_or(edge_sequence, |p| p.max(edge_sequence)));

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
        max_consecutive_stale: max_stale_run,
        longest_stall_seconds: longest_stall,
        target_duration,
        regressions,
        new_segments,
        observed_seconds: started.elapsed().as_secs_f64(),
        avg_latency: avg,
        min_latency: min,
        max_latency: max,
    })
}

impl LiveStats {
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        // A new segment is due every target duration; polling adds up to
        // half of one. Past 1.5x something upstream really did stall.
        if self.target_duration > 0.0 && self.longest_stall_seconds > 1.5 * self.target_duration {
            w.push(format!(
                "playlist stalled for {:.1}s ({} consecutive unchanged refreshes) against a {:.0}s target duration",
                self.longest_stall_seconds, self.max_consecutive_stale, self.target_duration
            ));
        }
        if self.regressions > 0 {
            w.push(format!(
                "playlist edge moved backwards {} time(s): an older copy was served after a newer one (inconsistent origin or CDN caching)",
                self.regressions
            ));
        }
        w
    }
}

pub fn stats_json(s: &LiveStats) -> Value {
    json!({
        "refreshes": s.refreshes,
        "stale_refreshes": s.stale_refreshes,
        "max_consecutive_stale": s.max_consecutive_stale,
        "longest_stall_seconds": s.longest_stall_seconds,
        "edge_regressions": s.regressions,
        "warnings": s.warnings(),
        "new_segments": s.new_segments,
        "observed_seconds": s.observed_seconds,
        "edge_lag_seconds": {
            "avg": s.avg_latency,
            "min": s.min_latency,
            "max": s.max_latency,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(longest_stall: f64, run: u32, regressions: u32) -> LiveStats {
        LiveStats {
            refreshes: 10,
            stale_refreshes: 5,
            max_consecutive_stale: run,
            longest_stall_seconds: longest_stall,
            target_duration: 6.0,
            regressions,
            new_segments: 5,
            observed_seconds: 30.0,
            avg_latency: None,
            min_latency: None,
            max_latency: None,
        }
    }

    #[test]
    fn alternating_stale_refreshes_are_normal() {
        // Polling at target/2: one stale refresh between segments is expected.
        assert!(stats(6.0, 1, 0).warnings().is_empty());
    }

    #[test]
    fn long_stall_and_regression_warn() {
        let w = stats(15.0, 4, 1).warnings();
        assert!(w.iter().any(|m| m.contains("stalled for 15.0s")));
        assert!(w.iter().any(|m| m.contains("backwards")));
    }
}
