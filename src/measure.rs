use std::time::Instant;

use anyhow::Result;
use m3u8_rs::MediaPlaylist;
use reqwest::blocking::Client;
use serde_json::{json, Value};
use url::Url;

use crate::fetch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    MpegTs,
    Fmp4,
    Unknown,
}

impl Container {
    fn sniff(bytes: &[u8]) -> Self {
        if bytes.len() >= 189 && bytes[0] == 0x47 && bytes[188] == 0x47 {
            return Container::MpegTs;
        }
        if bytes.len() >= 8 {
            match &bytes[4..8] {
                b"ftyp" | b"styp" | b"moof" | b"sidx" | b"moov" => return Container::Fmp4,
                _ => {}
            }
        }
        Container::Unknown
    }

    pub fn label(self) -> &'static str {
        match self {
            Container::MpegTs => "MPEG-TS",
            Container::Fmp4 => "fMP4/CMAF",
            Container::Unknown => "unknown",
        }
    }
}

pub struct SegmentMeasure {
    pub uri: String,
    pub bytes: u64,
    pub duration: f64,
    pub ttfb_ms: f64,
    pub total_ms: f64,
    /// Download throughput over the whole request, in bits per second.
    pub throughput_bps: f64,
    /// Actual media bitrate: payload bits over declared segment duration.
    pub bitrate_bps: f64,
}

pub struct MeasureReport {
    pub container: Container,
    pub init_segment: bool,
    pub segments: Vec<SegmentMeasure>,
    /// Aggregate measured bitrate across all sampled segments.
    pub measured_bitrate_bps: f64,
    /// Highest single-segment bitrate seen.
    pub peak_bitrate_bps: f64,
    pub avg_ttfb_ms: f64,
}

/// Download the `count` newest segments of a media playlist and measure them.
///
/// TTFB is the time until response headers arrive; throughput is computed
/// over the full body download. The actual media bitrate (bytes against the
/// declared EXTINF duration) is what should be compared with the variant's
/// BANDWIDTH attribute: RFC 8216 §4.3.4.2 defines BANDWIDTH as an upper
/// bound of the peak segment bit rate.
pub fn measure(
    client: &Client,
    playlist_url: &Url,
    pl: &MediaPlaylist,
    count: usize,
) -> Result<MeasureReport> {
    let take = count.min(pl.segments.len());
    let newest = &pl.segments[pl.segments.len() - take..];

    let mut container = Container::Unknown;
    let mut init_segment = false;

    // CMAF/fMP4 variants carry an init segment in EXT-X-MAP. The parser
    // attaches it to the segment that follows the tag, which is usually the
    // oldest one still in the playlist, so search them all.
    if let Some(map) = pl.segments.iter().find_map(|s| s.map.as_ref()) {
        init_segment = true;
        let init_url = fetch::resolve(playlist_url, &map.uri)?;
        let fetched = fetch::fetch(client, &init_url)?;
        container = Container::sniff(&fetched.bytes);
    }

    let mut segments = Vec::with_capacity(take);
    for seg in newest {
        let seg_url = fetch::resolve(playlist_url, &seg.uri)?;
        let start = Instant::now();
        let resp = client.get(seg_url.as_str()).send()?.error_for_status()?;
        let ttfb = start.elapsed();
        let body = resp.bytes()?;
        let total = start.elapsed();

        if container == Container::Unknown {
            container = Container::sniff(&body);
        }

        let bytes = body.len() as u64;
        let duration = seg.duration as f64;
        segments.push(SegmentMeasure {
            uri: seg.uri.clone(),
            bytes,
            duration,
            ttfb_ms: ttfb.as_secs_f64() * 1000.0,
            total_ms: total.as_secs_f64() * 1000.0,
            throughput_bps: bytes as f64 * 8.0 / total.as_secs_f64().max(1e-9),
            bitrate_bps: bytes as f64 * 8.0 / duration.max(1e-9),
        });
    }

    let total_bytes: u64 = segments.iter().map(|s| s.bytes).sum();
    let total_duration: f64 = segments.iter().map(|s| s.duration).sum();
    let measured = total_bytes as f64 * 8.0 / total_duration.max(1e-9);
    let peak = segments.iter().map(|s| s.bitrate_bps).fold(0.0, f64::max);
    let avg_ttfb = if segments.is_empty() {
        0.0
    } else {
        segments.iter().map(|s| s.ttfb_ms).sum::<f64>() / segments.len() as f64
    };

    Ok(MeasureReport {
        container,
        init_segment,
        segments,
        measured_bitrate_bps: measured,
        peak_bitrate_bps: peak,
        avg_ttfb_ms: avg_ttfb,
    })
}

pub fn report_json(r: &MeasureReport) -> Value {
    json!({
        "container": r.container.label(),
        "init_segment": r.init_segment,
        "sampled_segments": r.segments.len(),
        "measured_bitrate_bps": r.measured_bitrate_bps as u64,
        "peak_segment_bitrate_bps": r.peak_bitrate_bps as u64,
        "avg_ttfb_ms": r.avg_ttfb_ms,
        "segments": r.segments.iter().map(|s| json!({
            "uri": s.uri,
            "bytes": s.bytes,
            "duration": s.duration,
            "ttfb_ms": s.ttfb_ms,
            "total_ms": s.total_ms,
            "throughput_bps": s.throughput_bps as u64,
            "bitrate_bps": s.bitrate_bps as u64,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_mpeg_ts() {
        let mut bytes = vec![0u8; 376];
        bytes[0] = 0x47;
        bytes[188] = 0x47;
        assert_eq!(Container::sniff(&bytes), Container::MpegTs);
    }

    #[test]
    fn sniffs_fmp4() {
        let mut bytes = vec![0u8; 16];
        bytes[4..8].copy_from_slice(b"styp");
        assert_eq!(Container::sniff(&bytes), Container::Fmp4);
    }
}
