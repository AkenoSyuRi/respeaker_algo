# Repository Guidelines

## Project Structure & Module Organization

This is a Windows-only Rust 2024 CLI for ReSpeaker Mic Array v2.0 recording and built-in algorithm pipelines. `src/main.rs` defines the recording CLI. `src/recorder.rs` coordinates fixed 16 kHz/6ch WASAPI capture, channel splitting, and raw WAV output; `src/audio.rs` defines the `CaptureBlock` and the fixed 16 kHz/6ch device contract. `src/pipeline.rs` parses module configuration; `src/pipeline_worker.rs` runs the configured modules in a dedicated algorithm thread, assembles fixed 256-frame hops from capture blocks, and manages the bounded queue lifecycle (queue `Full` only disables the pipeline; raw recording continues). `src/beamformer/` implements Delay-and-Sum / robust superdirective MVDR weights and the streaming WOLA STFT; it consumes the DOA internal angle, while the viewer only publishes the external angle, and `compare_wav = true` writes `*_respeaker_bf.wav` as a stereo comparison (left = mic1 × gain, right = BF output × gain). `src/web.rs` serves the embedded DOA SSE viewer from `web/`; `src/wasapi.rs` owns exclusive capture; `src/wav.rs` writes PCM WAV files.

DOA code lives under `src/doa/`: framing, geometry, SRP-PHAT processing, tracking, and runtime output are separated by module. Unit tests are colocated in `#[cfg(test)]` modules. Design requirements are documented in `docs/plans/`. Generated recordings and build artifacts belong under `target/` and must not be committed.

## Build, Test, and Development Commands

- `cargo fmt --all -- --check` verifies Rust formatting.
- `cargo clippy --all-targets --all-features -- -D warnings` rejects lint warnings.
- `cargo test --all-targets` runs deterministic tests without requiring an audio device.
- `cargo build --release` builds the optimized CLI.
- `cargo run -- --duration 10` performs a pure-recording Windows hardware smoke test.
- `cargo run -- --duration 10 --pipeline-config configs/doa.toml` records while running DOA and its local Web Viewer.
- `cargo run --release -- --duration 10 --pipeline-config configs/bf_fixed.toml` records while running the fixed-direction beamformer.
- `cargo run --release -- --duration 10 --pipeline-config configs/doa_bf.toml` records while running DOA steering the beamformer.

Run the full four-command validation sequence before submitting changes.

## Coding Style & Naming Conventions

Use standard `rustfmt` output and four-space indentation. Follow Rust naming conventions: `snake_case` for functions/modules, `CamelCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Keep changes surgical and reuse existing buffers in real-time paths. Do not run FFT, SRP, Kalman, or file I/O inside audio callbacks. Preserve the `ch0 / ch1..ch4 / ch5` channel contract and keep all pipeline modules disabled unless a configuration is supplied.

## Testing Guidelines

Add focused tests beside the affected module. Name tests after observable behavior, such as `frame_boundaries` or `coast_then_search`. DSP tests must use deterministic synthetic signals and circular-angle assertions. Cover malformed interleaved lengths, zero/low-level input, coordinate geometry, and 0°/360° crossings. Hardware-dependent behavior must be reported separately; automated tests must remain sound-card independent.

## Commit & Pull Request Guidelines

Recent commits use Conventional Commit subjects with concise Chinese summaries, for example `feat(doa): 集成 ReSpeaker 实时声源定位` and `fix(doa): 修正 ReSpeaker 阵列坐标`. Use a short subject plus 2–6 meaningful body bullets for nontrivial changes.

Pull requests should describe behavior changes, affected CLI/config/output contracts, validation commands and results, and whether ReSpeaker hardware testing was performed. Update `README.md`, `configs/`, and relevant plans when changing flags, module schemas, channel geometry, DSP defaults, or output formats.
