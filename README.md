# ZLMediaKit-RS

A high-performance, multi-protocol streaming media server written in Rust. A reimplementation of [ZLMediaKit](https://github.com/ZLMediaKit/ZLMediaKit) that enables live stream publishing and playback across RTMP, RTSP, HTTP-FLV, and HLS protocols.

## Quick Start

```bash
cargo build --release
./target/release/zlmediakit
```

Default ports: RTMP `1935`, RTSP `554`, HTTP `80` / API `8081`.

Config at `conf/config.toml` or override per flag:

```bash
./target/release/zlmediakit --log-level debug --rtmp-port 19350
```

## Test a Stream

```bash
# Publish via RTMP
ffmpeg -re -i input.mp4 -c copy -f flv rtmp://localhost:1935/live/stream

# Play via RTSP
ffplay rtsp://localhost:554/live/stream

# Play via HTTP-FLV
ffplay http://localhost:80/live/stream.flv

# Play via HLS (open in browser or ffplay)
ffplay http://localhost:80/live/stream/hls.m3u8
```

## Architecture

Eight crates behind a single binary:

| Crate | Role |
|-------|------|
| `core` | `MediaSource` (shared frame bus), `GopCache`, `SessionManager`, `EventBus` |
| `codec` | H264/H265/AAC/G711 bitstream parsers |
| `rtmp` | RTMP handshake, chunk stream, AMF0 encode/decode, publish/play |
| `rtsp` | RTSP DESCRIBE/SETUP/PLAY, TCP/UDP interleaved |
| `http` | HTTP server, HTTP-FLV streaming, HLS serving, REST API |
| `flv` | FLV muxer/demuxer, `FlvRecorder` |
| `hls` | HLS muxer (`TsWriter`), segmenter, `HlsRecorder` |
| `server` | Binary entrypoint, config, recorder supervisor |

All protocols converge on `MediaSource`: a publisher pushes `MediaFrame`s via `publish_and_cache()`; players call `subscribe()` to receive the same frames regardless of protocol.

## Status

- **RTMP**: Publish and play fully working
- **RTSP**: TCP/UDP play working
- **HTTP-FLV**: Play working
- **HLS**: In progress — segments are produced; SPS/PPS and AAC config frame injection into TS segments being finalized

## License

MIT
