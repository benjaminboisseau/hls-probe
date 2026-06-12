# hls-probe

*[Version française](README.fr.md)*

A small command-line probe for HLS streams. Point it at any `.m3u8` URL and it tells you what is in there and whether it is healthy:

- **Full inventory** — the video ABR ladder (bandwidth, average bandwidth, resolution, frame rate, codecs, rendition groups), audio renditions (language, channels, accessibility characteristics), subtitle renditions, and i-frame trick-play playlists, each listed in its own section.
- **Conformance checks** — segment durations exceeding `EXT-X-TARGETDURATION` (RFC 8216 §4.3.3.1), variants referencing undeclared rendition groups, missing `CODECS`/`RESOLUTION` attributes, duplicate bandwidths, discontinuity counts, wall-clock drift between `EXT-X-PROGRAM-DATE-TIME` and summed segment durations.
- **Segment measurement** (`--measure`) — downloads sample segments and reports TTFB, download time, throughput, container format (MPEG-TS vs fMP4/CMAF, init segment detection), and the measured bitrate against the declared `BANDWIDTH` (which RFC 8216 §4.3.4.2 defines as the peak segment bit rate — exceeding it is a real conformance problem).
- **Live monitoring** (`--live`) — polls a live media playlist at half the target duration, counts new and stale refreshes, and estimates the playlist edge's lag behind real time from PDT tags when present.
- **CDN edge test** (`--edge-test`) — measures the cache-miss penalty the first viewer per edge node pays on every newly published segment: fetches each new segment immediately, then the same URL one target duration later, and compares TTFB. Reads CDN cache-decision headers (`X-Cache`, `Age`, `cf-cache-status`...) to prove the miss/hit, including Akamai's debug Pragma.
- **JSON output** (`--json`) and a non-zero exit code on error-level findings, so it drops into monitoring scripts and CI without ceremony.

Born from operating live distribution platforms (IPTV/OTT head-ends) where the first question is always: *is the origin producing a sane playlist, and how far behind live are we?*

## Install

```
cargo install --git https://github.com/benjaminboisseau/hls-probe
```

Or clone and `cargo build --release`; the binary lands in `target/release/hls-probe`.

## Usage

Inventory a master playlist:

```
$ hls-probe https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8
master playlist: https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8
5 variant(s):
    6221600 bps  1920x1080   -      mp4a.40.2,avc1.640028
    2149280 bps  1280x720    -      mp4a.40.2,avc1.64001f
    ...
  no issues found
```

Watch a live stream advance (follows the top-bandwidth variant of a master):

```
$ hls-probe --live -n 10 https://example.com/live/master.m3u8
refresh  1/10: edge seq 927754341, 312 segments, edge lag 11.2s
...
live: 10 refreshes over 14s, 5 new segment(s), 0 stale refresh(es)
  edge lag: avg 11.7s, min 9.4s, max 14.1s (PDT-based)
```

Analyze every variant and emit JSON:

```
$ hls-probe --all --json https://example.com/master.m3u8
```

| Flag | Effect |
| --- | --- |
| `--live` | poll a live playlist and measure refresh behaviour |
| `-n, --refreshes <N>` | number of refreshes to observe (default 10) |
| `--all` | analyze every variant of a master playlist |
| `--measure` | download sample segments: TTFB, throughput, real vs declared bitrate, container |
| `-s, --segments <N>` | number of segments to sample with `--measure` (default 3) |
| `--edge-test` | measure the CDN cache-miss penalty on freshly published segments |
| `-p, --pairs <N>` | number of fresh/warmed pairs to collect with `--edge-test` (default 5) |
| `--json` | machine-readable output |

Exit codes: `0` clean or warnings only, `2` at least one error-level finding, `1` network/parse failure.

## Roadmap

- Low-latency HLS (partial segments, preload hints, blocking playlist reload)
- Multi-variant live monitoring with alignment comparison
- Audio/subtitle rendition playlist analysis

## License

MIT
