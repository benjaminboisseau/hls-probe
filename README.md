# hls-probe

*[Version française](README.fr.md)*

A small command-line probe for HLS streams. Point it at any `.m3u8` URL and it tells you what is in there and whether it is healthy:

- **Variant inventory** — bandwidth, resolution, frame rate and codecs of every variant in a master playlist.
- **Conformance checks** — segment durations exceeding `EXT-X-TARGETDURATION` (RFC 8216 §4.3.3.1), missing `CODECS`/`RESOLUTION` attributes, duplicate bandwidths, discontinuity counts, wall-clock drift between `EXT-X-PROGRAM-DATE-TIME` and summed segment durations.
- **Live monitoring** (`--live`) — polls a live media playlist at half the target duration, counts new and stale refreshes, and estimates the playlist edge's lag behind real time from PDT tags.
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
| `--json` | machine-readable output |

Exit codes: `0` clean or warnings only, `2` at least one error-level finding, `1` network/parse failure.

## Roadmap

- Segment download checks (availability, size vs declared duration, TS/fMP4 sniffing)
- Low-latency HLS (partial segments, preload hints)
- Multi-variant live monitoring with alignment comparison

## License

MIT
