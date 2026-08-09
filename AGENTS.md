# Repository Guidelines

## Project Structure & Module Organization

This is a Rust 2024 CLI for ReSpeaker Mic Array v2.0 recording and real-time direction-of-arrival (DOA) estimation. `src/main.rs` defines the CLI. `src/recorder.rs` coordinates capture, channel splitting, WAV output, and optional DOA processing. `src/audio.rs` provides the cross-platform CPAL backend; `src/wasapi.rs` contains Windows-exclusive six-channel capture; `src/wav.rs` writes PCM WAV files.

DOA code lives under `src/doa/`: framing, geometry, SRP-PHAT processing, tracking, and runtime output are separated by module. Unit tests are colocated in `#[cfg(test)]` modules. Design requirements are documented in `docs/plans/`. Generated recordings and build artifacts belong under `target/` and must not be committed.

## Build, Test, and Development Commands

- `cargo fmt --all -- --check` verifies Rust formatting.
- `cargo clippy --all-targets --all-features -- -D warnings` rejects lint warnings.
- `cargo test --all-targets` runs deterministic tests without requiring an audio device.
- `cargo build --release` builds the optimized CLI.
- `cargo run -- list-devices --verbose` inspects available inputs.
- `cargo run -- --backend wasapi --doa --duration 10` performs a Windows hardware smoke test.

Run the full four-command validation sequence before submitting changes.

## Coding Style & Naming Conventions

Use standard `rustfmt` output and four-space indentation. Follow Rust naming conventions: `snake_case` for functions/modules, `CamelCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Keep changes surgical and reuse existing buffers in real-time paths. Do not run FFT, SRP, Kalman, or file I/O inside audio callbacks. Preserve the `ch0 / ch1..ch4 / ch5` channel contract and keep DOA disabled by default.

## Testing Guidelines

Add focused tests beside the affected module. Name tests after observable behavior, such as `frame_boundaries` or `coast_then_search`. DSP tests must use deterministic synthetic signals and circular-angle assertions. Cover malformed interleaved lengths, zero/low-level input, coordinate geometry, and 0°/360° crossings. Hardware-dependent behavior must be reported separately; automated tests must remain sound-card independent.

## Commit & Pull Request Guidelines

Recent commits use Conventional Commit subjects with concise Chinese summaries, for example `feat(doa): 集成 ReSpeaker 实时声源定位` and `fix(doa): 修正 ReSpeaker 阵列坐标`. Use a short subject plus 2–6 meaningful body bullets for nontrivial changes.

Pull requests should describe behavior changes, affected CLI/output contracts, validation commands and results, and whether ReSpeaker hardware testing was performed. Update `README.md` and relevant plans when changing flags, channel geometry, DSP defaults, or output formats.
