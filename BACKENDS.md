# Implementing a `MemoryProvider` backend

This guide is for contributors and downstream teams who want to plug a different
storage engine into the CEL memory stack — PostgreSQL, DuckDB, Redis, a hosted
vector service, or something internal — **without changing agent code**.

You do **not** implement storage inside `cel-memory`. That crate owns the
**contract** only. Each backend ships as its **own crate** that depends on
`cel-memory` and implements [`MemoryProvider`].

[`MemoryProvider`]: https://docs.rs/cel-memory/latest/cel_memory/trait.MemoryProvider.html

---

## Mental model

```text
  agent / CLI / server / tests
           │
           │  depends on trait + value types only
           ▼
      cel-memory  ─── MemoryProvider, MemoryQuery, hooks, Summarizer, …
           ▲
           │  implements
           │
   ┌───────┴────────┬──────────────────┐
   │                │                  │
BasicMemoryProvider  cel-memory-sqlite   your-cel-memory-postgres
(in-crate reference) (local file + vec)  (pgvector + tsvector, …)
```

Application code should take `&dyn MemoryProvider` (or `Arc<dyn MemoryProvider>`)
at boundaries. See [`examples/backend_swap.rs`](examples/backend_swap.rs).

Wrappers and decorators (logging, metrics, policy) implement the same trait and
delegate to an inner provider. See [`examples/custom_provider.rs`](examples/custom_provider.rs).

---

## What belongs where

| Layer | Crate | Owns |
|-------|-------|------|
| Contract | `cel-memory` | `MemoryProvider`, chunks, queries, sessions, errors, `Embedder`, `Summarizer`, `MemoryWriteHook` |
| Reference impl | `cel-memory` (`BasicMemoryProvider`) | In-memory behavior for tests and docs |
| Production local backend | [`cel-memory-sqlite`](https://github.com/dimpagk92/cel-memory-sqlite) | SQLite schema, migrations, hybrid retrieval, embedder seam |
| Your backend | **new crate** (e.g. `cel-memory-postgres`) | Your DB client, schema, indexes, retrieval strategy |

**Keep out of `cel-memory`:** SQL drivers, migrations, embedding runtimes, connection
pools, and vendor-specific vector/FTS extensions.

**Keep in your crate:** everything that touches your storage engine.

---

## Recommended crate layout

```text
cel-memory-postgres/          # name is up to you; prefix helps discoverability
├── Cargo.toml                # depends on cel-memory = "0.1"
├── README.md                 # connection string, feature flags, ops notes
├── migrations/               # sqlx / refinery / diesel migrations
├── src/
│   ├── lib.rs
│   ├── error.rs              # map storage errors → MemoryError
│   ├── embedder.rs           # optional: wrap or extend cel_memory::Embedder
│   ├── schema.rs             # table definitions (conceptual map from sqlite)
│   └── provider.rs           # PostgresMemoryProvider + MemoryProvider impl
└── tests/
    ├── smoke.rs              # integration tests against real or testcontainer DB
    └── conformance.rs        # shared behavioral checks (see below)
```

Publish independently on crates.io. Point `repository` and `documentation` at
your GitHub repo and docs.rs page, matching the other CEL crates.

---

## Implementation path (phased)

Work in this order so you always have something runnable and testable.

### Phase 0 — Scaffold

1. Create a new crate with `cel-memory` as the only CEL dependency (use crates.io
   versions, not path deps, unless developing locally).
2. Define `PostgresMemoryProvider` (or your name) with `open` / `connect` that
   runs migrations and returns `Self`.
3. Add one conformance test: `Arc<dyn MemoryProvider>` can `write` + `get` + `stats`.
   Copy the pattern from [`cel-memory-sqlite/tests/swap.rs`](https://github.com/dimpagk92/cel-memory-sqlite/blob/main/tests/swap.rs).

### Phase 1 — Core persistence (read/write path)

Implement the minimum surface agents need for a single turn:

| Method | Notes |
|--------|-------|
| `write` | Assign `id` (UUID v7 recommended), `created_at`, default tier; run `MemoryWriteHook` before persist |
| `write_batch` | Optional optimization; sequential `write` is acceptable initially |
| `get` | By chunk id |
| `retrieve` | **Hard part** — see [Retrieval](#retrieval) below; start with lexical-only if needed |
| `open_session`, `get_session`, `list_sessions`, `close_session`, `rename_session` | Session table + outcome enum |
| `stats` | Counts + `embedding_model` when applicable |

At end of Phase 1, an agent can remember and recall across turns.

### Phase 2 — Lifecycle and hygiene

| Method | Notes |
|--------|-------|
| `pin`, `delete`, `delete_matching`, `purge_all` | Respect `MemoryPredicate::is_empty` → no-op for `delete_matching` |
| `record_access` | Optional access log; drives importance in full backends |
| `update_importance`, `supersede` | Persist fields on chunk rows |
| `export` | Serialize matching chunks/sessions/logs into `ExportBundle` |
| `run_aging_sweep` | Retention policy; match semantics documented on `BasicMemoryProvider` / sqlite |

### Phase 3 — Summarization (optional but expected for “full” backends)

Inject [`Summarizer`](https://docs.rs/cel-memory/latest/cel_memory/trait.Summarizer.html)
via `with_summarizer`, same pattern as `SqliteMemoryProvider`:

| Method | Without summarizer | With summarizer |
|--------|-------------------|-----------------|
| `summarize_session` | `Err(NotImplemented)` | Writes `JobSummary`, links members |
| `rollup_day` / `rollup_day_forced` | `Err(NotImplemented)` | Writes `Rollup` with day metadata |
| `rollup_rule_week` / `rollup_rule_week_forced` | `Err(NotImplemented)` | Groups `ChunkKind::Fire` by rule + week |

Reference behavior and edge cases live in
[`cel-memory-sqlite/tests/smoke.rs`](https://github.com/dimpagk92/cel-memory-sqlite/blob/main/tests/smoke.rs)
(search for `summarize_session`, `rollup_day`).

### Phase 4 — Embeddings maintenance

| Method | Notes |
|--------|-------|
| `re_embed_all` | Re-embed all stored chunks when `target_model` matches your embedder; return `ReEmbedReport` |

---

## Schema mapping (conceptual)

`cel-memory-sqlite` is the reference schema. Your backend should preserve the
**same entities**, not necessarily the same SQL dialect:

| Entity | Purpose |
|--------|---------|
| Chunks | All memory rows (`MemoryChunk` fields) |
| Vectors | Embedding per chunk for semantic retrieval |
| Lexical index | FTS / tsvector / inverted index for keyword leg |
| Sessions | `MemorySession` lifecycle |
| Summary members | Links rollup/summary chunk → source chunk ids |
| Access log | Optional; `record_access` |
| Eviction log | Optional; audit trail on deletes |

Inspect [`cel-memory-sqlite/migrations/`](https://github.com/dimpagk92/cel-memory-sqlite/tree/main/migrations)
for column names and JSON metadata conventions (`rollup_date`, `rollup_rule_id`,
`rule_id` on fire chunks, etc.).

---

## Retrieval

`retrieve(MemoryQuery)` is the main backend-specific design choice.

**Contract (all backends):**

- Honor `query.k`, `CallerScope`, `kinds`, time bounds, `session_id`,
  `project_root_prefix`, `include_rollups`, `min_importance`.
- Return chunks in **descending relevance** order.
- Empty `query.text` → `InvalidArgument`.

**Reference algorithm** (sqlite): hybrid **vector + FTS + recency**, weighted by
[`RetrievalProfile`](https://docs.rs/cel-memory/latest/cel_memory/enum.RetrievalProfile.html),
fused with reciprocal-rank fusion (RRF), with a short-TTL cache invalidated on
writes.

You may:

- **Match sqlite semantics** — best for drop-in replacement.
- **Start simpler** — e.g. lexical-only or vector-only, document the gap in your
  README until hybrid lands.
- **Delegate vectors** — Pinecone, Qdrant, pgvector, etc. — as long as the
  `MemoryQuery` contract is honored at the trait boundary.

Weights per profile are defined in `cel-memory-sqlite` (`retrieval_weights`); copy
those constants if you want compatible ranking behavior.

---

## Cross-cutting seams (implement like sqlite)

### Write hook

Before every persist, consult optional `MemoryWriteHook`:

- `Allow` → store normally
- `Redact { reason }` → do **not** persist; return synthetic chunk with
  `embedding_model: "none"` (see sqlite `write`)

### Embedder

The [`Embedder`] trait lives in `cel-memory` (since **0.2.0**). Backends take
`Arc<dyn Embedder>` at construction and use [`MockEmbedder`] in tests.
Production embedders (e.g. `FastEmbedEmbedder` in `cel-memory-sqlite`) can live
in backend crates.

[`Embedder`]: https://docs.rs/cel-memory/latest/cel_memory/trait.Embedder.html
[`MockEmbedder`]: https://docs.rs/cel-memory/latest/cel_memory/struct.MockEmbedder.html

Embed at write time; store model name + dimension on the chunk row.

Backend integration tests should call [`assert_write_get_stats`] from
`cel_memory::conformance` (re-exported at the crate root) so every persistence
layer honors the same write/get/stats contract.

[`assert_write_get_stats`]: https://docs.rs/cel-memory/latest/cel_memory/fn.assert_write_get_stats.html

### Errors

Map storage failures to [`MemoryError::Storage`]. Use
[`MemoryError::NotFound`], [`InvalidArgument`], [`NotImplemented`] consistently
with `BasicMemoryProvider` and sqlite so callers can branch predictably.

---

## Backend-specific starting points

These are **suggested stacks**, not requirements.

### PostgreSQL (`cel-memory-postgres`)

| Concern | Typical choice |
|---------|----------------|
| Driver | `sqlx` or `tokio-postgres` |
| Vectors | [`pgvector`](https://github.com/pgvector/pgvector) |
| Lexical | `tsvector` + GIN index |
| Migrations | `sqlx migrate` / refinery |
| Deploy | connection pool, SSL, multi-tenant by `caller_id` / schema |

**Why first:** mature ops story, shared memory across services, backups, compliance.

### DuckDB (`cel-memory-duckdb`)

| Concern | Notes |
|---------|-------|
| Deploy | embedded file or in-process, similar *feel* to sqlite |
| Vectors | check current DuckDB vector extension support for your target version |
| Lexical | DuckDB FTS / full-text features differ from FTS5 — plan explicitly |

**Why second:** good for local analytics-heavy workloads; less standard as a
shared agent-memory service.

### Redis / key-value / hosted vector DB

Implement `MemoryProvider` with your index as the retrieval engine. Persist full
`MemoryChunk` JSON in Redis hashes or object storage; use the hosted service for
`retrieve` candidate generation. Document consistency and durability trade-offs.

---

## Conformance testing

There is no published conformance crate yet. Until one exists, new backends should:

1. **Trait-object smoke test** — `Arc<dyn MemoryProvider>` write/get/stats (see
   sqlite `swap.rs`).
2. **Behavioral parity tests** — port high-value cases from sqlite `smoke.rs`:
   caller scope, shareable chunks, session lifecycle, summarizer paths you support.
3. **Compare against `BasicMemoryProvider`** for methods that are intentionally
   simple (aging sweep, empty predicate delete guard).

Optional: expose a `#[doc(hidden)] fn open_for_test()` that uses an ephemeral DB
(testcontainers, `:memory:`, temp dir) like sqlite's `open_in_memory`.

We welcome a shared `cel-memory-conformance` test harness in the ecosystem; it
is not a blocker for shipping your crate.

---

## Integration with the rest of CEL

| Consumer | How it uses memory |
|----------|-------------------|
| Agent / runtime | `Arc<dyn MemoryProvider>` in app state |
| [`cel-brief`](https://github.com/dimpagk92/cel-brief) | `MemorySource` over any provider (`memory` feature) |
| Your CLI | Construct provider at startup; no CLI required in `cel-memory` itself |

Keep LLM prompt assembly in `cel-brief`, not in your backend crate.

---

## Naming and publishing checklist

- Crate name: `cel-memory-<engine>` or your org prefix (`acme-memory-postgres`).
- README: connection setup, feature flags, retrieval strategy, known gaps vs sqlite.
- `Cargo.toml`: `repository`, `documentation`, `keywords` including `ai`, `memory`, `agent`.
- Changelog: call out retrieval semantics and any intentional differences from sqlite.
- Do **not** fork `cel-memory` — depend on it from crates.io.

---

## Getting help

- **Reference impl:** [`dimpagk92/cel-memory-sqlite`](https://github.com/dimpagk92/cel-memory-sqlite)
- **Trait docs:** [docs.rs/cel-memory](https://docs.rs/cel-memory)
- **Questions / RFCs:** open an issue on [`dimpagk92/cel-memory`](https://github.com/dimpagk92/cel-memory) with the label `backend` if available, or describe your target engine and retrieval plan.

If you publish a backend crate, open a PR to add it to the “Community backends”
list in this repo's README (section below) so others can discover it.
