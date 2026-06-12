mod analyze;
mod fetch;
mod live;

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
            let mut out = json!({
                "type": "master",
                "url": fetched.final_url.as_str(),
                "fetch_ms": fetched.elapsed.as_millis() as u64,
                "variants": report.variants.iter().map(|v| json!({
                    "uri": v.uri,
                    "bandwidth": v.bandwidth,
                    "resolution": v.resolution,
                    "codecs": v.codecs,
                    "frame_rate": v.frame_rate,
                })).collect::<Vec<_>>(),
                "issues": analyze::issues_json(&report.issues),
            });

            let mut variant_reports = Vec::new();
            if args.all || args.live {
                for v in &report.variants {
                    let v_url = fetch::resolve(&fetched.final_url, &v.uri)?;
                    if args.live {
                        // Follow the top-bandwidth variant only.
                        run_media(&client, &v_url, &args)?;
                        return Ok(());
                    }
                    let media = fetch_media(&client, &v_url)?;
                    let r = analyze::analyze_media(&media);
                    variant_reports.push((v.uri.clone(), r));
                }
            }

            if args.json {
                if !variant_reports.is_empty() {
                    out["variant_reports"] = variant_reports
                        .iter()
                        .map(|(uri, r)| media_json(uri, r))
                        .collect::<Vec<_>>()
                        .into();
                }
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                print_master(&fetched.final_url, &report);
                for (uri, r) in &variant_reports {
                    println!("\n--- variant: {uri}");
                    print_media(r);
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
        if let Some(stats) = &live_stats {
            out["live"] = live::stats_json(stats);
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print_media(&report);
        if let Some(stats) = &live_stats {
            print_live(stats);
        }
    }
    exit_with(&report.issues);
    Ok(())
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
    println!("{} variant(s):", r.variants.len());
    for v in &r.variants {
        println!(
            "  {:>9} bps  {:<11} {:<6} {}",
            v.bandwidth,
            v.resolution.as_deref().unwrap_or("?x?"),
            v.frame_rate.map_or(String::from("-"), |f| format!("{f}fps")),
            v.codecs.as_deref().unwrap_or("(no codecs)"),
        );
    }
    print_issues(&r.issues);
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
