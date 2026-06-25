# Changelog

All notable changes to `cel-memory` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Pre-1.0 releases were developed privately before the first public crates.io
line at `0.1.5`.

## [Unreleased]

### Added
- [`BACKENDS.md`](BACKENDS.md) — community guide for implementing alternate
  `MemoryProvider` storage engines in separate crates.

## [0.1.8] — 2026-06-25

### Added
- `BasicMemoryProvider::with_summarizer` and in-memory implementations of
  `summarize_session`, `rollup_day`, `rollup_rule_week`, and `re_embed_all`.

## [0.1.7] — 2026-06-25

### Changed
- Added crates.io metadata, README badges, and Clippy in CI.
- Removed orphan MIT license file; Apache-2.0 only.

## [0.1.6] — 2026-06-25

### Added
- Standalone GitHub repository at `https://github.com/dimpagk92/cel-memory`.
- Additional examples: `backend_swap`, `write_hook`, and `custom_provider`.

### Changed
- Renamed `ChunkSource::Cortex` to `ChunkSource::Perception` (deprecated alias retained).
- Published as a standalone crate on crates.io.

## [0.1.0-pre] — 2026-05-23

### Added
- `MemoryProvider` trait — the locked contract between memory backends and
  callers (agents, CLIs, servers, apps, test harnesses).
- Value types: `MemoryChunk`, `NewMemoryChunk`, `ChunkKind`, `ChunkSource`,
  `MemoryTier`, `MemorySession`, `NewMemorySession`, `SessionOutcome`,
  `MemoryQuery`, `MemoryPredicate`, `RetrievalProfile`, `CallerScope`.
- `BasicMemoryProvider` — in-process reference implementation. Useful for
  tests and as a documentation of the trait contract.
- `MemoryWriteHook` trait — governance seam consulted before every write.
  Lets a rule engine redact or veto persistence without coupling the
  memory provider to a specific rule format.
- `MemoryError` — self-contained `thiserror` enum. No runtime-specific
  re-exports.
- `examples/basic.rs` — runnable end-to-end example using
  `BasicMemoryProvider`. Builds with only this crate's declared deps.

### Notes
- Public API exposes no runtime-domain types. Future PRs may break the trait
  during the `0.1.0-pre` series; the trait stabilizes at `0.1.0`.
