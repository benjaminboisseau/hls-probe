# hls-probe

*[Version française](README.fr.md)*

A small command-line probe for HLS streams. Point it at any `.m3u8` URL and it tells you what is in there and whether it is healthy:

- **Full inventory** — the video ABR ladder (bandwidth, average bandwidth, resolution, frame rate, codecs, rendition groups), audio renditions (language, channels, accessibility characteristics), subtitle renditions, and i-frame trick-play playlists, each listed in its own section.
- **Conformance checks** — segment durations exceeding `EXT-X-TARGETDURATION` (RFC 8216 §4.3.3.1), variants referencing undeclared rendition groups, missing `CODECS`/`RESOLUTION` attributes, duplicate bandwidths, discontinuity counts, wall-clock drift between `EXT-X-PROGRAM-DATE-TIME` and summed segment durations.
- **Segment measurement** (`--measure`) — downloads sample segments and reports TTFB, download time, throughput, container format (MPEG-TS vs fMP4/CMAF, init segment detection), and the measured bitrate against the declared `BANDWIDTH` (which RFC 8216 §4.3.4.2 defines as the peak segment bit rate — exceeding it is a real conformance problem).
- **Origin cacheability audit** (part of `--measure`) — reports the cache-relevant headers segments actually carry (`ETag` strong/weak, `Last-Modified`, `Accept-Ranges`, `Cache-Control`) and warns when they will hurt on a real CDN: Google Media CDN refuses to cache objects over 1 MiB without a validator, and Google Cloud CDN only does chunked cache fill when the origin sends `Accept-Ranges: bytes` plus a strong `ETag` *and* `Last-Modified`. Origins built for CloudFront (which caches on `Cache-Control` alone) routinely fail this test.
- **Live monitoring** (`--live`) — polls a live media playlist at half the target duration, reports the longest time the playlist went without publishing a segment (warning past 1.5 target durations), flags an edge that moves backwards (an older copy served after a newer one), and estimates the playlist edge's lag behind real time from PDT tags when present.
- **CDN edge test** (`--edge-test`) — measures the cache-miss penalty the first viewer per edge node pays on every newly published segment: fetches each new segment immediately, then the same URL one target duration later, and compares TTFB. When the CDN states its cache decision (`Cache-Status` per RFC 9211, `X-Cache`, `cf-cache-status`, Akamai's debug Pragma), the verdict is built on that statement rather than on timing: a confirmed miss-then-hit penalty, an edge that was already warm, or — the one worth an alert — segments that miss on the repeat request too, meaning nothing is being cached.
- **JSON output** (`--json`) and a non-zero exit code on error-level findings — in the master playlist or in any variant analyzed with `--all` — so it drops into monitoring scripts and CI without ceremony.

Born from operating live distribution platforms (IPTV/OTT head-ends) where the first question is always: *is the origin producing a sane playlist, and how far behind live are we?*

## Install

```
cargo install --git https://github.com/benjaminboisseau/hls-probe
```

Or clone and `cargo build --release`; the binary lands in `target/release/hls-probe`. Prebuilt binaries for Linux, macOS and Windows are attached to each [release](https://github.com/benjaminboisseau/hls-probe/releases).

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
live: 10 refreshes over 14s, 5 new segment(s), longest stall 4.0s (5 of 10 refreshes unchanged, at most 1 in a row)
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
| `-H, --header <NAME: VALUE>` | add a request header to every request, curl style (repeatable) |
| `-t, --timeout <SECONDS>` | give up on a request after this long, body included (default 30) |

Custom headers apply to playlist and segment requests alike — handy for token-protected origins (`-H "Authorization: Bearer ..."`), for reaching an origin directly behind a virtual host (`-H "Host: origin.example"`), or for seeing what a real player would get (`-H "User-Agent: AppleCoreMedia/..."`). A user-supplied `User-Agent` replaces the default `hls-probe/<version>`.

Exit codes: `0` clean or warnings only, `2` at least one error-level finding, `1` network/parse failure.

## Roadmap

- Low-latency HLS (partial segments, preload hints, blocking playlist reload)
- Multi-variant live monitoring with alignment comparison
- Audio/subtitle rendition playlist analysis

## License

MIT
