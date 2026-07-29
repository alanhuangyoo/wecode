# Third-party notices

WeCode contains vendored source from the projects below. The vendored code is kept in
`third_party/` so its provenance and license remain visible.

## xai-org/grok-build

Source snapshot: `5da6962e4adb9c857f3def762542b52b4ec3e522`

Copyright 2023-2026 SpaceXAI

The following component is copied without functional changes:

- `third_party/xai-token-estimation/src/lib.rs`, originally
  `crates/codegen/xai-token-estimation/src/lib.rs`

The following component is packaged as a small standalone crate and has minor module-path changes:

- `third_party/codex-apply-patch/`, originally the pure parser, fuzzy matcher, and apply engine under
  `crates/codegen/xai-grok-tools/src/implementations/codex/apply_patch/`

These components are licensed under Apache License 2.0. Their license copies are stored alongside
the vendored source.

## openai/codex

Copyright 2025 OpenAI

Grok Build's `codex/apply_patch` implementation is a Rust port derived from OpenAI Codex
`codex-rs/apply-patch`. It retains the Codex patch grammar and fuzzy line matching behavior.
The code is licensed under Apache License 2.0.

The WeCode workspace adapter in `src/patch.rs` is new code. It adds path containment checks,
symlink escape protection, and filesystem I/O around the vendored pure engine.
