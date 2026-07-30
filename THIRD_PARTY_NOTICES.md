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

Source snapshot: `6219b7c40fc9c702c0aef9964e72b492558f60e41`

Grok Build's `codex/apply_patch` implementation is a Rust port derived from OpenAI Codex
`codex-rs/apply-patch`. It retains the Codex patch grammar and fuzzy line matching behavior.
The code is licensed under Apache License 2.0.

The WeCode workspace adapter in `src/patch.rs` is new code. It adds path containment checks,
symlink escape protection, and filesystem I/O around the vendored pure engine.

`src/markdown_render.rs` is adapted from the Codex TUI Markdown renderer introduced in
commit `8068cc75f8ad6d71c0c35b4b3109633b6edb7269`. WeCode removes Codex-specific file citation
handling and updates the parser integration for pulldown-cmark 0.13.

`src/frontmatter.rs` is adapted from the tolerant YAML scalar parsing and repair path in
`codex-rs/core-skills/src/loader.rs`. WeCode exposes only the shared string-field subset needed by
prompt commands and skills.

The interactive prompt's autonomy, persistence, short-request, and blocker-recovery rules are
adapted from Codex's GPT-5 model instructions. The OpenAI provider's explicit reasoning-effort
configuration and user-shell execution follow Codex's model request and shell runtime design.

## xai-org/grok-build TUI viewport

Copyright 2023-2026 SpaceXAI

`src/viewport.rs` is adapted from the scroll and follow-mode state in
`crates/codegen/xai-grok-pager/src/scrollback/state`. WeCode extracts only the generic viewport
state and removes pager-specific selection, sticky-header, and appearance behavior.

Both adapted source files are licensed under Apache License 2.0.

## anomalyco/opencode

Copyright 2025 opencode

`src/bash_arity.rs` is adapted from `packages/opencode/src/permission/arity.ts` at commit
`eca5e68a5ea7f2f54ec0d81a46a41110c43e62b4`. It retains OpenCode's longest-prefix command arity
algorithm and command table, represented as a Rust match table.

The interactive prompt's uncertainty-first investigation rule and compatible global instruction
discovery follow OpenCode's system prompt and `packages/opencode/src/session/instruction.ts`.

This component is licensed under the MIT License.

## badlogic/pi-mono

Copyright 2025 Mario Zechner

`src/compaction.rs` is adapted from Pi's
`packages/agent/src/harness/compaction/compaction.ts` at commit
`bb226f9c1f38d3c029156a690e97bbfc602336b9`. It retains the structured checkpoint prompts,
iterative previous-summary update, structural history cut, and retained recent tail, adapted to
WeCode's provider-neutral Rust message model.

`src/harness.rs`, `src/tool_registry.rs`, and the batch path in `src/agent.rs` adapt Pi's
`packages/agent/src/agent-loop.ts` tool execution contract. Tool calls are prepared in provider
order, independent parallel-capable tools may be mixed in one batch, and completed results are
written back in the original call order. Each result is bound directly to its provider call ID;
interruption repair is reserved for calls that genuinely did not finish.

The shell tool description follows Pi's Bash tool capability wording: commands execute from the
current working directory and return captured stdout and stderr.

This component is licensed under the MIT License.
