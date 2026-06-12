mod analyze;
mod fetch;
mod live;
mod measure;

use anyhow::{bail, Result};
use clap::Parser;
use m3u8_rs::Playlist;
use serde_json::json;
use url::Url;

use analyze::Severity;

/// Probe HLS streams: variant inventory, conformance checks, live-edge lag.
#[derive(Parser)]
#[command(name = "hls-probe", version, about)]
struct Args {
    /// URL of a master or media playlist (.m3u8)
    url: String,

    /// Poll a live media playlist and measure refresh behaviour
    #[arg(short, long)]
    live: bool,

    /// Number of playlist refreshes to observe with --live
    #[arg(short = 'n', long, default_value_t = 10)]
    refreshes: u32,

    /// Also analyze every variant listed in a master playlist
    #[arg(short, long)]
    all: bool,

    /// Download sample segments and measure TTFB, throughput and real bitrate
    #[arg(short, long)]
    measure: bool,

    /// Number of segments to download with --measure
    #[arg(short = 's', long, default_value_t = 3)]
    segments: usize,

    /// Emit JSON instead of human-readable text
    #[arg(short, long)]
    json: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let url = Url::parse(&args.url)?;
    let client = fetch::client()?;

    let fetched = fetch::fetch(&client, &url)?;
    let playlist = match m3u8_rs::parse_playlist_res(&fetched.bytes) {
        Ok(p) => p,
        Err(e) => bail!("not a valid M3U8 playlist: {e}"),
    };

    match playlist {
        Playlist::MasterPlaylist(master) => {
            let report = analyze::analyze_master(&master);
            let variant_json = |v: &analyze::VariantInfo| {
                json!({
                    "uri": v.uri,
                    "bandwidth": v.bandwidth,
                    "average_bandwidth": v.average_bandwidth,
                    "resolution": v.resolution,
                    "codecs": v.codecs,
                    "frame_rate": v.frame_rate,
                    "audio_group": v.audio_group,
                    "subtitles_group": v.subtitles_group,
                })
            };
            let rendition_json = |r: &analyze::RenditionInfo| {
                json!({
                    "group_id": r.group_id,
                    "name": r.name,
                    "language": r.language,
                    "uri": r.uri,
                    "default": r.default,
                    "channels": r.channels,
                    "characteristics": r.characteristics,
                })
            };
            let mut out = json!({
                "type": "master",
                "url": fetched.final_url.as_str(),
                "fetch_ms": fetched.elapsed.as_millis() as u64,
                "variants": report.variants.iter().map(variant_json).collect::<Vec<_>>(),
                "iframe_playlists": report.iframe_variants.iter().map(variant_json).collect::<Vec<_>>(),
                "audio_renditions": report.audio.iter().map(rendition_json).collect::<Vec<_>>(),
                "subtitle_renditions": report.subtitles.iter().map(rendition_json).collect::<Vec<_>>(),
                "issues": analyze::issues_json(&report.issues),
            });

            if args.live {
                // Follow the top-bandwidth variant only.
                if let Some(top) = report.variants.first() {
                    let v_url = fetch::resolve(&fetched.final_url, &top.uri)?;
                    run_media(&client, &v_url, &args)?;
                }
                return Ok(());
            }

            let mut variant_reports = Vec::new();
            if args.all {
                for v in &report.variants {
                    let v_url = fetch::resolve(&fetched.final_url, &v.uri)?;
                    let media = fetch_media(&client, &v_url)?;
                    let r = analyze::analyze_media(&media);
                    let m = if args.measure {
                        Some(measure::measure(&client, &v_url, &media, args.segments)?)
                    } else {
                        None
                    };
                    variant_reports.push((v.uri.clone(), r, m, v.bandwidth));
                }
            } else if args.measure {
                if let Some(top) = report.variants.first() {
                    let v_url = fetch::resolve(&fetched.final_url, &top.uri)?;
                    let media = fetch_media(&client, &v_url)?;
                    let r = analyze::analyze_media(&media);
                    let m = measure::measure(&client, &v_url, &media, args.segments)?;
                    variant_reports.push((top.uri.clone(), r, Some(m), top.bandwidth));
                }
            }

            if args.json {
                if !variant_reports.is_empty() {
                    out["variant_reports"] = variant_reports
                        .iter()
                        .map(|(uri, r, m, _)| {
                            let mut v = media_json(uri, r);
                            if let Some(m) = m {
                                v["measure"] = measure::report_json(m);
                            }
                            v
                        })
                        .collect::<Vec<_>>()
                        .into();
                }
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                print_master(&fetched.final_url, &report);
                for (uri, r, m, declared) in &variant_reports {
                    println!("\n--- variant: {uri}");
                    print_media(r);
                    if let Some(m) = m {
                        print_measure(m, Some(*declared));
                    }
                }
            }
            exit_with(&report.issues);
        }
        Playlist::MediaPlaylist(_) => {
            run_media(&client, &fetched.final_url, &args)?;
        }
    }
    Ok(())
}

fn fetch_media(client: &reqwest::blocking::Client, url: &Url) -> Result<m3u8_rs::MediaPlaylist> {
    let fetched = fetch::fetch(client, url)?;
    match m3u8_rs::parse_playlist_res(&fetched.bytes) {
        Ok(Playlist::MediaPlaylist(p)) => Ok(p),
        Ok(Playlist::MasterPlaylist(_)) => bail!("{url} is a master playlist, expected media"),
        Err(e) => bail!("{url}: not a valid M3U8 playlist: {e}"),
    }
}

fn run_media(client: &reqwest::blocking::Client, url: &Url, args: &Args) -> Result<()> {
    let media = fetch_media(client, url)?;
    let report = analyze::analyze_media(&media);

    let measured = if args.measure {
        Some(measure::measure(client, url, &media, args.segments)?)
    } else {
        None
    };

    let live_stats = if args.live {
        if !report.is_live {
            bail!("--live requested but the playlist has EXT-X-ENDLIST (VOD)");
        }
        Some(live::monitor(client, url, args.refreshes, args.json)?)
    } else {
        None
    };

    if args.json {
        let mut out = media_json(url.as_str(), &report);
        if let Some(m) = &measured {
            out["measure"] = measure::report_json(m);
        }
        if let Some(stats) = &live_stats {
            out["live"] = live::stats_json(stats);
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print_media(&report);
        if let Some(m) = &measured {
            print_measure(m, None);
        }
        if let Some(stats) = &live_stats {
            print_live(stats);
        }
    }
    exit_with(&report.issues);
    Ok(())
}

fn print_measure(m: &measure::MeasureReport, declared_bandwidth: Option<u64>) {
    println!(
        "measure: {} segment(s) sampled, container {}{}",
        m.segments.len(),
        m.container.label(),
        if m.init_segment { " (init segment present)" } else { "" },
    );
    println!(
        "  measured bitrate {:.0} kbps (peak segment {:.0} kbps), avg TTFB {:.0} ms",
        m.measured_bitrate_bps / 1000.0,
        m.peak_bitrate_bps / 1000.0,
        m.avg_ttfb_ms,
    );
    for s in &m.segments {
        println!(
            "  {:>8} KiB in {:>5.0} ms (ttfb {:>4.0} ms) -> {:>6.0} kbps throughput  {}",
            s.bytes / 1024,
            s.total_ms,
            s.ttfb_ms,
            s.throughput_bps / 1000.0,
            s.uri,
        );
    }
    if let Some(declared) = declared_bandwidth {
        // RFC 8216 §4.3.4.2: BANDWIDTH is an upper bound of the peak segment
        // bit rate; a sampled segment exceeding it is a real conformance issue.
        if m.peak_bitrate_bps > declared as f64 {
            println!(
                "  [warning] peak segment bitrate {:.0} kbps exceeds declared BANDWIDTH {:.0} kbps",
                m.peak_bitrate_bps / 1000.0,
                declared as f64 / 1000.0,
            );
        }
    }
}

fn media_json(uri: &str, r: &analyze::MediaReport) -> serde_json::Value {
    json!({
        "type": "media",
        "url": uri,
        "live": r.is_live,
        "version": r.version,
        "target_duration": r.target_duration,
        "media_sequence": r.media_sequence,
        "segments": r.segment_count,
        "total_duration": r.total_duration,
        "avg_segment_duration": r.avg_segment_duration,
        "discontinuities": r.discontinuities,
        "program_date_time": r.has_program_date_time,
        "issues": analyze::issues_json(&r.issues),
    })
}

fn print_master(url: &Url, r: &analyze::MasterReport) {
    println!("master playlist: {url}");
    println!("video variants ({}):", r.variants.len());
    for v in &r.variants {
        println!(
            "  {:>7} kbps{}  {:<11} {:<6} {}{}",
            v.bandwidth / 1000,
            v.average_bandwidth
                .map_or(String::new(), |a| format!(" (avg {})", a / 1000)),
            v.resolution.as_deref().unwrap_or("?x?"),
            v.frame_rate.map_or(String::from("-"), |f| format!("{f}fps")),
            v.codecs.as_deref().unwrap_or("(no codecs)"),
            match (&v.audio_group, &v.subtitles_group) {
                (Some(a), Some(s)) => format!("  [audio:{a} subs:{s}]"),
                (Some(a), None) => format!("  [audio:{a}]"),
                (None, Some(s)) => format!("  [subs:{s}]"),
                (None, None) => String::new(),
            },
        );
    }
    if !r.audio.is_empty() {
        println!("audio renditions ({}):", r.audio.len());
        for a in &r.audio {
            print_rendition(a);
        }
    }
    if !r.subtitles.is_empty() {
        println!("subtitle renditions ({}):", r.subtitles.len());
        for s in &r.subtitles {
            print_rendition(s);
        }
    }
    if !r.iframe_variants.is_empty() {
        println!("i-frame trick-play playlists ({}):", r.iframe_variants.len());
        for v in &r.iframe_variants {
            println!(
                "  {:>7} kbps  {:<11} {}",
                v.bandwidth / 1000,
                v.resolution.as_deref().unwrap_or("?x?"),
                v.codecs.as_deref().unwrap_or(""),
            );
        }
    }
    print_issues(&r.issues);
}

fn print_rendition(a: &analyze::RenditionInfo) {
    println!(
        "  {:<14} {:<4} {:<20}{}{}{}",
        a.group_id,
        a.language.as_deref().unwrap_or("-"),
        a.name,
        if a.default { " DEFAULT" } else { "" },
        a.channels
            .as_deref()
            .map_or(String::new(), |c| format!(" {c}ch")),
        a.characteristics
            .as_deref()
            .map_or(String::new(), |c| format!(" ({c})")),
    );
}

fn print_media(r: &analyze::MediaReport) {
    println!(
        "media playlist: {} (version {})",
        if r.is_live { "LIVE" } else { "VOD" },
        r.version.map_or(String::from("?"), |v| v.to_string()),
    );
    println!(
        "  {} segments, {:.1}s total, avg {:.2}s, target duration {}s",
        r.segment_count, r.total_duration, r.avg_segment_duration, r.target_duration,
    );
    println!(
        "  media sequence {}, {} discontinuity(ies), PDT {}",
        r.media_sequence,
        r.discontinuities,
        if r.has_program_date_time { "yes" } else { "no" },
    );
    print_issues(&r.issues);
}

fn print_live(s: &live::LiveStats) {
    println!(
        "live: {} refreshes over {:.0}s, {} new segment(s), {} stale refresh(es)",
        s.refreshes, s.observed_seconds, s.new_segments, s.stale_refreshes,
    );
    match (s.avg_latency, s.min_latency, s.max_latency) {
        (Some(avg), Some(min), Some(max)) => println!(
            "  edge lag: avg {avg:.1}s, min {min:.1}s, max {max:.1}s (PDT-based)"
        ),
        _ => println!("  edge lag: unknown (stream has no EXT-X-PROGRAM-DATE-TIME)"),
    }
}

fn print_issues(issues: &[analyze::Issue]) {
    if issues.is_empty() {
        println!("  no issues found");
        return;
    }
    for i in issues {
        let tag = match i.severity {
            Severity::Warning => "warning",
            Severity::Error => "ERROR",
        };
        println!("  [{tag}] {}", i.message);
    }
}

/// Exit non-zero when any error-level issue was found, so the probe can sit
/// in monitoring scripts and CI checks.
fn exit_with(issues: &[analyze::Issue]) {
    if issues.iter().any(|i| i.severity == Severity::Error) {
        std::process::exit(2);
    }
}
