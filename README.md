# cel-memory

[![crates.io](https://img.shields.io/crates/v/cel-memory.svg)](https://crates.io/crates/cel-memory)
[![docs.rs](https://docs.rs/cel-memory/badge.svg)](https://docs.rs/cel-memory)
[![CI](https://github.com/dimpagk92/cel-memory/actions/workflows/ci.yml/badge.svg)](https://github.com/dimpagk92/cel-memory/actions/workflows/ci.yml)

Backend-agnostic memory traits and value types for AI agents.

`cel-memory` is the contract between an agent and an arbitrary persistence
layer. The trait is small enough to drop in your own backend (file, SQLite,
Redis, Mem0, Hindsight, etc.) without changing agent code. The companion
[`cel-memory-sqlite`](https://crates.io/crates/cel-memory-sqlite) crate provides a local SQLite
implementation.

## Purpose

Use `cel-memory` when agent code needs durable, scoped retrieval but should not
depend on a storage engine. Callers depend on `MemoryProvider`; backends decide
how chunks are stored, embedded, indexed, summarized, and aged. Attach a
[`Summarizer`](https://docs.rs/cel-memory/latest/cel_memory/trait.Summarizer.html)
with [`BasicMemoryProvider::with_summarizer`] to enable session summaries and rollups.

**Status:** v0.2.0 on [crates.io](https://crates.io/crates/cel-memory) — the `MemoryProvider` trait surface is stable. Two implementations ship against it: `BasicMemoryProvider` (in-crate, in-memory reference) and [`cel-memory-sqlite`](https://crates.io/crates/cel-memory-sqlite) (SQLite + vector + FTS, hybrid retrieval). LLM summarizers live in [`cel-summarizer`](https://crates.io/crates/cel-summarizer).

## What's Included

- `MemoryProvider` trait — async interface every backend implements.
- Value types: `MemoryChunk`, `ChunkKind`, `MemoryTier`, `MemoryQuery`, `MemorySession`, etc.
- `BasicMemoryProvider` — in-memory reference impl. Useful for tests and as the conformance reference for new backends.
- `MemoryWriteHook` trait — governance hook every backend should consult before persisting (lets a rule engine redact or veto writes).
- `MemoryError` — self-contained error type.

## Out Of Scope

- Storage. See [`cel-memory-sqlite`](https://crates.io/crates/cel-memory-sqlite) for SQLite + vector retrieval.
- Embedding runtimes. The [`Embedder`] trait is defined here; ONNX/fastembed backends ship in companion crates.
- LLM-call retrieval logic. See `cel-brief` for "retrieve memory + assemble into an LLM prompt."

## Example

```rust
use cel_memory::{
    BasicMemoryProvider, ChunkKind, ChunkSource, MemoryProvider,
    NewMemoryChunk, MemoryQuery, CallerScope, RetrievalProfile,
};
use serde_json::json;

let memory = BasicMemoryProvider::new();

memory.write(NewMemoryChunk {
    kind: ChunkKind::Chat,
    source: ChunkSource::Embedded,
    caller_id: "my-agent".into(),
    content: "User prefers dry-run mode".into(),
    session_id: None,
    project_root: None,
    metadata: json!(null),
    importance: None,
    shareable: false,
    pinned: false,
}).await?;

let hits = memory.retrieve(MemoryQuery {
    text: "dry-run".into(),
    caller_scope: CallerScope::Own,
    caller_id: "my-agent".into(),
    k: 5,
    // ...
    profile: RetrievalProfile::AgentChatTurn,
    kinds: None, since: None, until: None,
    session_id: None, project_root_prefix: None,
    include_rollups: true, min_importance: None,
}).await?;
```

Runnable examples:

```sh
cargo run --example basic
cargo run --example backend_swap
cargo run --example write_hook
cargo run --example custom_provider
```

- `basic` uses the in-memory reference provider end to end.
- `backend_swap` shows application code written against `MemoryProvider`.
- `write_hook` shows policy/redaction before persistence.
- `custom_provider` shows a provider wrapper that implements the trait.

## Implementing another backend

Storage engines (PostgreSQL, DuckDB, Redis, hosted vector DBs, etc.) belong in
**separate crates** that implement [`MemoryProvider`](https://docs.rs/cel-memory/latest/cel_memory/trait.MemoryProvider.html).
Do not add drivers or SQL to this repo.

See **[BACKENDS.md](BACKENDS.md)** for a phased implementation guide, schema
mapping from `cel-memory-sqlite`, retrieval expectations, and conformance
testing. Published community backends can be listed there and in the table below.

| Backend | Crate | Status |
|---------|-------|--------|
| In-memory (reference) | `cel-memory` (`BasicMemoryProvider`) | maintained here |
| SQLite + vector + FTS | [`cel-memory-sqlite`](https://crates.io/crates/cel-memory-sqlite) | maintained |
| *your engine* | *your crate* | community |

## Comparable libraries

| | cel-memory | Hindsight | Mem0 | Letta |
|---|---|---|---|---|
| Language | Rust | Python | Python | Python |
| Local-first | ✓ | ✓ | partial | ✓ |
| Pluggable backend | ✓ (trait) | partial | ✗ | ✗ |
| Governance hooks | ✓ | ✗ | ✗ | partial |
| Per-caller scoping | ✓ | ✗ | partial | partial |
| Sessions | ✓ | ✓ | ✓ | ✓ |

## License

Apache-2.0
