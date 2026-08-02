# Continuous parser fuzzing

The fuzz package exercises the same public parsers used by the server:

- `flv`: incremental FLV header/tag parsing;
- `ps`: incremental MPEG-PS/PES parsing with a bounded pending buffer;
- `mp4`: complete MP4 box/sample-table parsing.

Run a target with nightly Rust and `cargo-fuzz`:

```bash
cargo install cargo-fuzz --locked
cargo +nightly fuzz run flv -- -max_total_time=60 -max_len=1048576
cargo +nightly fuzz run ps -- -max_total_time=60 -max_len=1048576
cargo +nightly fuzz run mp4 -- -max_total_time=60 -max_len=1048576
```

Crashes are written under `fuzz/artifacts/`; generated corpora and build output
are intentionally not committed.
