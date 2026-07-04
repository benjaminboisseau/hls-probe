use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use m3u8_rs::{MediaPlaylist, MediaSegment};
use reqwest::blocking::Client;
use reqwest::header::RANGE;
use serde_json::{json, Value};
use url::Url;

use crate::fetch;

/// Resolve each segment's `#EXT-X-BYTERANGE` to an absolute (offset, length)
/// pair. Per RFC 8216 §4.3.2.2, an omitted offset means "immediately after
/// the previous sub-range requested for the same URI", so this has to walk
/// the whole playlist in order rather than just the sampled tail.
fn resolve_byte_ranges(segments: &[MediaSegment]) -> Vec<Option<(u64, u64)>> {
    let mut next_offset: HashMap<&str, u64> = HashMap::new();
    segments
        .iter()
        .map(|seg| {
            let br = seg.byte_range.as_ref()?;
            let offset = br
                .offset
                .unwrap_or_else(|| *next_offset.get(seg.uri.as_str()).unwrap_or(&0));
            next_offset.insert(seg.uri.as_str(), offset + br.length);
            Some((offset, br.length))
        })
        .collect()
}

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

/// Cache-relevant response headers observed on the sampled segments.
///
/// CDNs disagree on what an origin must send before they will cache large
/// objects. Google Cloud CDN only performs chunked cache fill (required for
/// objects over 1 MiB, and for anything over 10 MiB at all) when the origin
/// answers with `Accept-Ranges: bytes`, `Content-Length`, *and* both
/// `Last-Modified` and a strong `ETag`. Google Media CDN refuses to cache
/// objects over 1 MiB without a validator and byte-range support. CloudFront
/// needs none of this and caches on `Cache-Control` alone — which is why an
/// origin that looks healthy behind CloudFront can stop caching behind GCP.
#[derive(Debug, Default, Clone)]
pub struct OriginCacheability {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub cache_control: Option<String>,
    pub accept_ranges_bytes: bool,
    /// Largest single sampled segment, to compare against the 1 MiB rule.
    pub largest_segment_bytes: u64,
}

impl OriginCacheability {
    pub fn etag_is_strong(&self) -> bool {
        self.etag.as_deref().is_some_and(|v| !v.starts_with("W/"))
    }

    pub fn has_validator(&self) -> bool {
        self.etag.is_some() || self.last_modified.is_some()
    }

    /// Would Google Cloud CDN treat this origin as supporting byte ranges?
    pub fn gcp_range_capable(&self) -> bool {
        self.accept_ranges_bytes && self.etag_is_strong() && self.last_modified.is_some()
    }

    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        let over_1mib = self.largest_segment_bytes > 1_048_576;
        if over_1mib && !self.has_validator() {
            w.push(format!(
                "segments up to {} KiB carry no ETag or Last-Modified: Google Media CDN will not cache objects over 1 MiB without a validator",
                self.largest_segment_bytes / 1024
            ));
        }
        if over_1mib && !self.gcp_range_capable() {
            w.push(
                "origin fails Google Cloud CDN's byte-range test (needs Accept-Ranges: bytes + strong ETag + Last-Modified): no chunked cache fill, cacheable size capped at 10 MiB".to_string(),
            );
        }
        if self.cache_control.is_none() {
            w.push(
                "no Cache-Control on segments: CDNs in honor-origin mode will not cache them at all".to_string(),
            );
        }
        w
    }
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
    pub cacheability: OriginCacheability,
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
    let ranges = resolve_byte_ranges(&pl.segments);
    let newest = &pl.segments[pl.segments.len() - take..];
    let newest_ranges = &ranges[pl.segments.len() - take..];

    let mut container = Container::Unknown;
    let mut init_segment = false;

    // CMAF/fMP4 variants carry an init segment in EXT-X-MAP. The parser
    // attaches it to the segment that follows the tag, which is usually the
    // oldest one still in the playlist, so search them all.
    if let Some(map) = pl.segments.iter().find_map(|s| s.map.as_ref()) {
        init_segment = true;
        let init_url = fetch::resolve(playlist_url, &map.uri)?;
        let mut req = client.get(init_url.as_str());
        if let Some(br) = &map.byte_range {
            let offset = br.offset.unwrap_or(0);
            req = req.header(RANGE, format!("bytes={}-{}", offset, offset + br.length - 1));
        }
        let fetched = req.send()?.error_for_status()?.bytes()?;
        container = Container::sniff(&fetched);
    }

    let mut segments = Vec::with_capacity(take);
    let mut cacheability = OriginCacheability::default();
    for (seg, range) in newest.iter().zip(newest_ranges.iter()) {
        let seg_url = fetch::resolve(playlist_url, &seg.uri)?;
        let mut req = client.get(seg_url.as_str());
        if let Some((offset, length)) = range {
            req = req.header(RANGE, format!("bytes={}-{}", offset, offset + length - 1));
        }
        let start = Instant::now();
        let resp = req.send()?.error_for_status()?;
        let ttfb = start.elapsed();

        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        cacheability.etag = header("etag").or(cacheability.etag.take());
        cacheability.last_modified = header("last-modified").or(cacheability.last_modified.take());
        cacheability.cache_control = header("cache-control").or(cacheability.cache_control.take());
        // A 206 answer to our own Range request proves range support even if
        // the origin omits Accept-Ranges on partial responses.
        cacheability.accept_ranges_bytes |= header("accept-ranges").as_deref() == Some("bytes")
            || resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;

        let body = resp.bytes()?;
        let total = start.elapsed();

        if container == Container::Unknown {
            container = Container::sniff(&body);
        }

        let bytes = body.len() as u64;
        cacheability.largest_segment_bytes = cacheability.largest_segment_bytes.max(bytes);
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
        cacheability,
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
        "origin_cacheability": {
            "etag": r.cacheability.etag,
            "etag_strong": r.cacheability.etag_is_strong(),
            "last_modified": r.cacheability.last_modified,
            "cache_control": r.cacheability.cache_control,
            "accept_ranges_bytes": r.cacheability.accept_ranges_bytes,
            "largest_segment_bytes": r.cacheability.largest_segment_bytes,
            "gcp_cloud_cdn_range_capable": r.cacheability.gcp_range_capable(),
            "warnings": r.cacheability.warnings(),
        },
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

    fn seg(uri: &str, byte_range: Option<(u64, Option<u64>)>) -> MediaSegment {
        MediaSegment {
            uri: uri.to_string(),
            byte_range: byte_range.map(|(length, offset)| m3u8_rs::ByteRange { length, offset }),
            ..Default::default()
        }
    }

    #[test]
    fn resolves_explicit_offsets() {
        let segments = vec![
            seg("main.mp4", Some((5874288, Some(721)))),
            seg("main.mp4", Some((5863101, Some(5875009)))),
        ];
        let ranges = resolve_byte_ranges(&segments);
        assert_eq!(ranges, vec![Some((721, 5874288)), Some((5875009, 5863101))]);
    }

    #[test]
    fn resolves_omitted_offset_from_previous_range_same_uri() {
        // Per RFC 8216 4.3.2.2, a missing offset continues right after the
        // previous sub-range requested for the same URI.
        let segments = vec![
            seg("main.mp4", Some((1000, Some(0)))),
            seg("main.mp4", Some((500, None))),
            seg("other.mp4", Some((200, Some(50)))),
        ];
        let ranges = resolve_byte_ranges(&segments);
        assert_eq!(
            ranges,
            vec![Some((0, 1000)), Some((1000, 500)), Some((50, 200))]
        );
    }

    #[test]
    fn segments_without_byte_range_pass_through() {
        let segments = vec![seg("a.ts", None), seg("b.ts", None)];
        assert_eq!(resolve_byte_ranges(&segments), vec![None, None]);
    }

    #[test]
    fn weak_etag_is_not_strong() {
        let c = OriginCacheability {
            etag: Some("W/\"abc\"".to_string()),
            ..Default::default()
        };
        assert!(!c.etag_is_strong());
        assert!(c.has_validator());
    }

    #[test]
    fn gcp_range_capability_needs_all_three() {
        let full = OriginCacheability {
            etag: Some("\"abc\"".to_string()),
            last_modified: Some("Mon, 01 Jun 2026 00:00:00 GMT".to_string()),
            accept_ranges_bytes: true,
            ..Default::default()
        };
        assert!(full.gcp_range_capable());
        for missing in [
            OriginCacheability { etag: None, ..full.clone() },
            OriginCacheability { last_modified: None, ..full.clone() },
            OriginCacheability { accept_ranges_bytes: false, ..full.clone() },
            OriginCacheability { etag: Some("W/\"abc\"".to_string()), ..full.clone() },
        ] {
            assert!(!missing.gcp_range_capable());
        }
    }

    #[test]
    fn large_segments_without_validator_warn() {
        let c = OriginCacheability {
            cache_control: Some("max-age=1209600".to_string()),
            largest_segment_bytes: 3 * 1_048_576,
            ..Default::default()
        };
        let warnings = c.warnings();
        assert!(warnings.iter().any(|w| w.contains("Media CDN")));
        assert!(warnings.iter().any(|w| w.contains("Cloud CDN")));
    }

    #[test]
    fn small_segments_with_validator_are_quiet() {
        let c = OriginCacheability {
            etag: Some("\"abc\"".to_string()),
            last_modified: Some("Mon, 01 Jun 2026 00:00:00 GMT".to_string()),
            cache_control: Some("max-age=14".to_string()),
            accept_ranges_bytes: true,
            largest_segment_bytes: 500_000,
        };
        assert!(c.warnings().is_empty());
    }
}
