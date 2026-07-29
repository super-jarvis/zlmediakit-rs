# CODEBUDDY.md This file provides guidance to CodeBuddy when working with code in this repository.

## Commands

### Build the workspace
```bash
cargo build
```
Compiles all crates with default features. Use `cargo build --release` for an optimized binary (`target/release/zlmediakit`).

### Run the streaming server
```bash
cargo run --bin zlmediakit -- --config conf/config.toml
```
Defaults: RTMP `1935`, RTSP `554`, HTTP `80` (override per `conf/config.toml`). CLI flags `--rtmp-port`, `--rtsp-port`, `--http-port`, `-a/--api-port`, `--log-level` override config. Config file path defaults to `conf/config.toml`.

### Run all tests
```bash
cargo test
```
Runs unit tests across the workspace. The only in-tree test lives in `crates/core/src/media_source.rs` and exercises the broadcast delivery of `MediaSource`.

### Run a single test
```bash
cargo test -p zlmediakit-core test_broadcast_delivery
```
`-p <crate>` targets a member crate; the trailing argument filters by test name.

### Lint / format
```bash
cargo fmt --check && cargo clippy
```
`cargo fmt` enforces rustfmt; `cargo clippy` runs lints. CI-style checks should pass before committing.

### Functional smoke test (external)
```bash
python3 test/test_rtmp.py
```
Connects to `127.0.0.1:19350`, performs the RTMP handshake and a `connect` command. Note the test hardcodes port `19350` (different from the default `1935`), so start the server with `--rtmp-port 19350` for it to work.

## Architecture

This is a Rust reimplementation of ZLMediaKit — a multi-protocol streaming media server that lets a stream published over one protocol (e.g. RTMP) be consumed over any other (RTSP, HTTP-FLV, HLS). It is a Cargo workspace of eight member crates (see `Cargo.toml` `[workspace]`).

### The central abstraction: protocol-agnostic media routing
The whole system is built around one idea in `crates/core`: a stream is identified by `vhost/app/stream` and represented by a `MediaSource` (`crates/core/src/media_source.rs`). A `MediaSource` owns:
- a `broadcast::channel<MediaFrame>` (`frame_tx`) for live frame distribution to all current players, and
- a `GopCache` (`crates/core/src/gop_cache.rs`) that retains metadata/config frames and the most recent Group-of-Pictures so a late-joining player can start on a key frame.

A publisher pushes frames with `MediaSource::publish_and_cache(frame)`, which both broadcasts to live subscribers and appends to the GOP cache. A player calls `MediaSource::subscribe()` to receive the live stream and typically replays `get_latest_gop_frames()`/`get_cached_config_frames()` first. `MediaSourceManager` (`DashMap` keyed by `vhost/app/stream`) is the registry every protocol server shares via `Arc`. This single shared bus is what enables cross-protocol playback.

### `MediaFrame` — the universal media unit
`crates/core/src/media_frame.rs` defines `MediaFrame` (track id, `FrameType` Video/Audio/Metadata, `CodecId`, timestamps pts/dts, `Bytes` payload, key/config flags) plus `MediaInfo`/`TrackInfo`. All protocol crates convert their wire format into/from `MediaFrame`, so the rest of the system never deals with protocol specifics.

### Crate responsibilities
- **`core`** (foundation, no protocol logic): `MediaFrame`/`MediaInfo`, `MediaSource`/`MediaSourceManager`, the `MediaSink` trait (`crates/core/src/media_sink.rs` — an `async_trait` consumer abstraction with `on_frame`/`on_media_info`/`close`), `SessionManager` (`session.rs`), `EventBus` (`event_bus.rs`; broadcasts `Event`s like `StreamPublish`/`StreamPlay`/`NoReader` for monitoring), `ServerConfig` (TOML-loaded, `config.rs`), and the unified `Error`/`Result` (`error.rs`).
- **`codec`**: bitstream parsers (`h264`, `h265`, `aac`, `g711`) used when re-packaging or inspecting payloads. Pure parsing utilities, no I/O.
- **`rtmp`, `rtsp`, `http`** (protocol servers): each exposes `<Proto>Server` (binds a `TcpListener`, spawns a `<Proto>Session` per connection, sharing the `Arc<MediaSourceManager>`) and a `session.rs` that converts the wire protocol to/from `MediaFrame` and calls `get_or_create`/`publish_and_cache`/`subscribe` on `core`.
  - `rtmp`: `handshake.rs`, `amf.rs` (AMF0 encode/decode), `message.rs` (chunk/message parsing + encoding), `session.rs` routes `publish`/`play`/`createStream` commands.
  - `rtsp`: `parser.rs` (RTSP request/response line parsing) + `session.rs` (DESCRIBE/SETUP/PLAY/TEARDOWN, TCP interleaved mode per `rtsp.tcp_mode`).
  - `http`: `router.rs` + `session.rs` (HTTP request handling, used for HTTP-FLV / API / file serving).
- **`flv`, `hls`**: packaging layers. Each has a `muxer` (turn `MediaFrame`s into FLV tags / HLS `.ts` segments + `.m3u8`) and a `demuxer` (reverse). They consume from a `MediaSource`/`MediaSink` rather than from the network directly.
- **`server`** (binary `zlmediakit`): `src/main.rs` loads `ServerConfig`, builds one `Arc<MediaSourceManager>`, and spawns a Tokio task per enabled protocol server; `Ctrl+C` aborts all. This is the only crate with a `main`.

### Data flow (publish → play, cross-protocol)
1. Publisher connects to (say) RTMP; `RtmpSession` performs handshake, `connect`, `createStream`, `publish`, then feeds each video/audio `RtmpMessage` into a `MediaFrame` and calls `source.publish_and_cache(frame)`.
2. The `MediaSource` broadcasts the frame and caches it.
3. A player connects via RTSP/HLS/HTTP; its session resolves the same `MediaSource` via `MediaSourceManager::get`, replays the GOP cache, then subscribes to the broadcast — receiving the identical frames regardless of the playback protocol.
4. `EventBus` emits lifecycle events (publish/unpublish/play/stop/no-reader) throughout, which could drive recording or pruning.

### Configuration
`conf/config.toml` is the authoritative config; `ServerConfig` (`crates/core/src/config.rs`) declares every field with safe defaults (e.g. `secret`, per-protocol `enabled`/`port`/`ssl`, `record`). CLI flags in `server/src/main.rs` override the file. When adding config, add the field in `core` and a default fn there.

### Conventions
- All async I/O uses `tokio` (`features = ["full"]`); shared mutable state uses `DashMap`/`RwLock`/`parking_lot`, never `std` mutexes on async paths.
- Errors funnel into `zlmediakit_core::Error`; use `anyhow::Result` at server boundaries.
- Logging is via `tracing`; verbosity is controlled by `--log-level` / `RUST_LOG`. Frame paths are chatty at `debug`, so expect high-volume logs when debugging publish/play.
- Protocol crates must not hard-depend on each other; they only depend on `core` (+ `codec` where needed). Cross-protocol behavior emerges purely from the shared `MediaSourceManager`.
