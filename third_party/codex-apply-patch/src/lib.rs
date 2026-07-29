//! Pure Codex `apply_patch` parser and fuzzy matching engine.
//!
//! Vendored from xai-org/grok-build, which ports the corresponding OpenAI
//! Codex implementation. See the repository's `THIRD_PARTY_NOTICES.md`.

pub mod apply;
pub mod errors;
pub mod parser;
pub mod seek_sequence;

pub use apply::derive_new_contents;
pub use errors::{ApplyPatchError, ParseError};
pub use parser::{Hunk, ParsedPatch, UpdateFileChunk, parse_patch};
