//! cel-memory — the durable cross-turn memory contract for AI agents.
//!
//! This crate answers one question: **what should persist across turns?** It
//! owns the memory *contract* — the [`MemoryProvider`] trait plus the value
//! types every backend and caller share (chunks, sessions, queries, retrieval
//! profiles, caller scopes, write hooks, summaries, rollups, aging, export).
//! Storage backends implement the trait; callers depend only on it.
//!
//! The crate is deliberately narrow. It does **not** observe live environment
//! state and it does **not** assemble per-turn LLM prompts. `cel-memory` owns
//! persistence only.
//!
//! [`BasicMemoryProvider`] is the in-crate reference implementation — real
//! bodies for [`MemoryProvider::retrieve`], [`MemoryProvider::write`], session
//! lifecycle, export, stats, summarization, rollups, and re-embed metadata
//! updates when a [`Summarizer`] is attached; no-ops for
//! [`MemoryProvider::update_importance`] and [`MemoryProvider::supersede`].
//! A full storage backend (e.g. [`cel-memory-sqlite`](https://crates.io/crates/cel-memory-sqlite))
//! implements the same trait in a separate crate — see
//! [BACKENDS.md](https://github.com/dimpagk92/cel-memory/blob/main/BACKENDS.md).

#![deny(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod basic;
pub mod chunk;
pub mod conformance;
pub mod embedder;
pub mod error;
pub mod importance;
pub mod offdevice_hook;
pub mod ops;
pub mod provider;
pub mod query;
pub mod session;
pub mod summarizer;
pub mod write_hook;

// Convenient re-exports — the symbols every caller will name.
pub use basic::BasicMemoryProvider;
pub use chunk::{ChunkKind, ChunkSource, MemoryChunk, MemoryTier, NewMemoryChunk};
pub use conformance::{
    assert_retrieve_finds_written, assert_session_lifecycle, assert_summarize_session_roundtrip,
    assert_write_get_stats,
};
pub use embedder::{Embedder, EmbedderError, EmbedderResult, MockEmbedder};
pub use error::{MemoryError, Result};
pub use importance::score as score_importance;
pub use offdevice_hook::{
    ClosureOffdeviceHook, OffdeviceCallDescriptor, OffdeviceCallHook, OffdeviceDecision,
};
pub use ops::{
    AccessEntry, AgingReport, EvictionEntry, EvictionReason, ExportBundle, ExportFilter,
    MemoryStats, PurgeReport, ReEmbedReport,
};
pub use provider::MemoryProvider;
pub use query::{CallerScope, MemoryPredicate, MemoryQuery, RetrievalProfile};
pub use session::{MemorySession, NewMemorySession, SessionFilter, SessionOutcome};
pub use summarizer::{
    MockSummarizer, MockSummaryCall, Summarizer, SummarizerError, SummarizerResult, SummaryContext,
};
pub use write_hook::{ClosureHook, MemoryWriteHook, WriteDecision};
