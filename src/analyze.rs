use m3u8_rs::{MasterPlaylist, MediaPlaylist};
use serde_json::{json, Value};

/// A conformance or sanity finding, with a severity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub severity: Severity,
    pub message: String,
}

impl Issue {
    fn warn(message: impl Into<String>) -> Self {
        Issue {
            severity: Severity::Warning,
            message: message.into(),
        }
    }
    fn error(message: impl Into<String>) -> Self {
        Issue {
            severity: Severity::Error,
            message: message.into(),
        }
    }
}

pub struct MediaReport {
    pub is_live: bool,
    pub version: Option<usize>,
    pub target_duration: f64,
    pub media_sequence: u64,
    pub segment_count: usize,
    pub total_duration: f64,
    pub avg_segment_duration: f64,
    pub discontinuities: usize,
    pub has_program_date_time: bool,
    pub issues: Vec<Issue>,
}

/// Run RFC 8216 sanity checks on a media playlist.
pub fn analyze_media(pl: &MediaPlaylist) -> MediaReport {
    let mut issues = Vec::new();
    let target = pl.target_duration as f64;

    let segment_count = pl.segments.len();
    let total_duration: f64 = pl.segments.iter().map(|s| s.duration as f64).sum();
    let avg = if segment_count > 0 {
        total_duration / segment_count as f64
    } else {
        0.0
    };

    if segment_count == 0 {
        issues.push(Issue::error("playlist contains no segments"));
    }

    // RFC 8216 §4.3.3.1: each segment duration, rounded to the nearest
    // integer, must be <= EXT-X-TARGETDURATION.
    for (i, seg) in pl.segments.iter().enumerate() {
        let rounded = (seg.duration as f64).round();
        if rounded > target {
            issues.push(Issue::error(format!(
                "segment #{i} duration {:.3}s exceeds EXT-X-TARGETDURATION {}s",
                seg.duration, pl.target_duration
            )));
        }
    }

    let discontinuities = pl.segments.iter().filter(|s| s.discontinuity).count();
    if discontinuities > 0 {
        issues.push(Issue::warn(format!(
            "{discontinuities} EXT-X-DISCONTINUITY tag(s) present"
        )));
    }

    let pdt_count = pl
        .segments
        .iter()
        .filter(|s| s.program_date_time.is_some())
        .count();
    let has_pdt = pdt_count > 0;
    if !has_pdt && !pl.end_list {
        issues.push(Issue::warn(
            "no EXT-X-PROGRAM-DATE-TIME tags: live-edge latency cannot be measured",
        ));
    }

    // Wall-clock drift: compare the PDT span against the sum of segment
    // durations between the first and last dated segments.
    if let (Some(first), Some(last)) = (
        pl.segments.iter().find(|s| s.program_date_time.is_some()),
        pl.segments.iter().rev().find(|s| s.program_date_time.is_some()),
    ) {
        if let (Some(first_pdt), Some(last_pdt)) = (first.program_date_time, last.program_date_time)
        {
            let pdt_span = (last_pdt - first_pdt).num_milliseconds() as f64 / 1000.0;
            let first_idx = pl
                .segments
                .iter()
                .position(|s| s.program_date_time == Some(first_pdt))
                .unwrap_or(0);
            let last_idx = pl
                .segments
                .iter()
                .position(|s| s.program_date_time == Some(last_pdt))
                .unwrap_or(segment_count.saturating_sub(1));
            let dur_span: f64 = pl.segments[first_idx..last_idx]
                .iter()
                .map(|s| s.duration as f64)
                .sum();
            let drift = pdt_span - dur_span;
            if pdt_span > 0.0 && drift.abs() > target {
                issues.push(Issue::warn(format!(
                    "PDT span and summed segment durations drift by {drift:.1}s \
                     (possible timing gap or encoder clock issue)"
                )));
            }
        }
    }

    MediaReport {
        is_live: !pl.end_list,
        version: pl.version,
        target_duration: target,
        media_sequence: pl.media_sequence,
        segment_count,
        total_duration,
        avg_segment_duration: avg,
        discontinuities,
        has_program_date_time: has_pdt,
        issues,
    }
}

pub struct MasterReport {
    pub variants: Vec<VariantInfo>,
    pub issues: Vec<Issue>,
}

pub struct VariantInfo {
    pub uri: String,
    pub bandwidth: u64,
    pub resolution: Option<String>,
    pub codecs: Option<String>,
    pub frame_rate: Option<f64>,
}

pub fn analyze_master(pl: &MasterPlaylist) -> MasterReport {
    let mut issues = Vec::new();

    if pl.variants.is_empty() {
        issues.push(Issue::error("master playlist declares no variant streams"));
    }

    let mut variants: Vec<VariantInfo> = pl
        .variants
        .iter()
        .map(|v| VariantInfo {
            uri: v.uri.clone(),
            bandwidth: v.bandwidth,
            resolution: v.resolution.map(|r| format!("{}x{}", r.width, r.height)),
            codecs: v.codecs.clone(),
            frame_rate: v.frame_rate,
        })
        .collect();
    variants.sort_by(|a, b| b.bandwidth.cmp(&a.bandwidth));

    for v in &pl.variants {
        if v.codecs.is_none() {
            issues.push(Issue::warn(format!(
                "variant '{}' has no CODECS attribute (hurts player startup decisions)",
                v.uri
            )));
        }
        if v.resolution.is_none() {
            issues.push(Issue::warn(format!(
                "variant '{}' has no RESOLUTION attribute",
                v.uri
            )));
        }
    }

    let mut seen = std::collections::HashSet::new();
    for v in &pl.variants {
        if !seen.insert(v.bandwidth) {
            issues.push(Issue::warn(format!(
                "duplicate BANDWIDTH value {} across variants",
                v.bandwidth
            )));
        }
    }

    MasterReport { variants, issues }
}

pub fn issues_json(issues: &[Issue]) -> Value {
    Value::Array(
        issues
            .iter()
            .map(|i| {
                json!({
                    "severity": match i.severity { Severity::Warning => "warning", Severity::Error => "error" },
                    "message": i.message,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_media(src: &str) -> MediaPlaylist {
        match m3u8_rs::parse_playlist_res(src.as_bytes()).expect("parse") {
            m3u8_rs::Playlist::MediaPlaylist(p) => p,
            _ => panic!("expected media playlist"),
        }
    }

    #[test]
    fn flags_segment_longer_than_target_duration() {
        let pl = parse_media(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n\
             #EXTINF:6.0,\nseg0.ts\n#EXT-X-ENDLIST\n",
        );
        let report = analyze_media(&pl);
        assert!(report
            .issues
            .iter()
            .any(|i| i.severity == Severity::Error && i.message.contains("exceeds")));
    }

    #[test]
    fn vod_with_endlist_is_not_live() {
        let pl = parse_media(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n\
             #EXTINF:5.0,\nseg0.ts\n#EXTINF:5.0,\nseg1.ts\n#EXT-X-ENDLIST\n",
        );
        let report = analyze_media(&pl);
        assert!(!report.is_live);
        assert_eq!(report.segment_count, 2);
        assert!((report.total_duration - 10.0).abs() < 1e-6);
    }

    #[test]
    fn counts_discontinuities() {
        let pl = parse_media(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n\
             #EXTINF:5.0,\nseg0.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:5.0,\nseg1.ts\n",
        );
        let report = analyze_media(&pl);
        assert_eq!(report.discontinuities, 1);
        assert!(report.is_live);
    }
}
