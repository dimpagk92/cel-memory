//! `BasicMemoryProvider` — the v1 backing implementation.
//!
//! In-process, in-memory, deliberately simple. Implements retrieval, writes,
//! session lifecycle, simple deletes, export, stats, summarization, rollups,
//! and re-embed metadata updates when a [`Summarizer`] is attached; no-ops for
//! `update_importance` and `supersede`.
//!
//! Lexical retrieval only — substring + recency. A full storage backend
//! (e.g. the `cel-memory-sqlite` crate) replaces this with hybrid
//! (vector + FTS + recency) retrieval.
//!
//! The persistence layer here is `Arc<Mutex<State>>`. It is useful for tests,
//! examples, and lightweight agents that do not need durability across process
//! restarts.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{NaiveDate, Utc};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::chunk::{ChunkKind, ChunkSource, MemoryChunk, MemoryTier, NewMemoryChunk};
use crate::error::{MemoryError, Result};
use crate::ops::{
    AccessEntry, AgingReport, EvictionEntry, EvictionReason, ExportBundle, ExportFilter,
    MemoryStats, PurgeReport, ReEmbedReport,
};
use crate::provider::MemoryProvider;
use crate::query::{MemoryPredicate, MemoryQuery};
use crate::session::{MemorySession, NewMemorySession, SessionFilter, SessionOutcome};
use crate::{Summarizer, SummarizerError, SummaryContext};

/// The v1 backing implementation. Cheap to construct; `Clone` is shallow
/// (shares the underlying state). Safe to share across tasks.
#[derive(Clone)]
pub struct BasicMemoryProvider {
    state: Arc<Mutex<State>>,
    /// Optional pre-write hook (redaction, governance). When unset, every
    /// write proceeds verbatim.
    write_hook: Option<Arc<dyn crate::MemoryWriteHook>>,
    /// Optional summarizer for session summaries and rollups.
    summarizer: Option<Arc<dyn Summarizer>>,
}

impl Default for BasicMemoryProvider {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            write_hook: None,
            summarizer: None,
        }
    }
}

impl std::fmt::Debug for BasicMemoryProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BasicMemoryProvider")
            .field("state", &self.state)
            .field("write_hook", &self.write_hook.as_ref().map(|_| "<hook>"))
            .field(
                "summarizer",
                &self.summarizer.as_ref().map(|_| "<summarizer>"),
            )
            .finish()
    }
}

#[derive(Debug, Default)]
struct State {
    chunks: HashMap<String, MemoryChunk>,
    sessions: HashMap<String, MemorySession>,
    evictions: Vec<EvictionEntry>,
    accesses: Vec<AccessEntry>,
    /// rollup/summary id → member chunk ids (idempotent linking).
    summary_members: HashMap<String, Vec<String>>,
}

impl BasicMemoryProvider {
    /// Construct an empty provider. Equivalent to `Default::default()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a [`MemoryWriteHook`](crate::MemoryWriteHook) consulted
    /// before every write. Use this for policy-driven memory redaction or
    /// application-specific write filtering.
    pub fn with_write_hook(mut self, hook: Arc<dyn crate::MemoryWriteHook>) -> Self {
        self.write_hook = Some(hook);
        self
    }

    /// Attach a [`Summarizer`] used by [`MemoryProvider::summarize_session`],
    /// [`MemoryProvider::rollup_day`], and [`MemoryProvider::rollup_rule_week`].
    pub fn with_summarizer(mut self, summarizer: Arc<dyn Summarizer>) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    fn next_id() -> String {
        Uuid::now_v7().to_string()
    }

    fn metadata_str(chunk: &MemoryChunk, key: &str) -> Option<String> {
        chunk
            .metadata
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    async fn fetch_session_chunks(&self, session_id: &str) -> Result<Vec<MemoryChunk>> {
        let state = self.state.lock().await;
        let mut out: Vec<MemoryChunk> = state
            .chunks
            .values()
            .filter(|c| {
                c.session_id.as_deref() == Some(session_id) && c.kind != ChunkKind::JobSummary
            })
            .cloned()
            .collect();
        out.sort_by_key(|c| c.created_at);
        Ok(out)
    }

    async fn fetch_day_chunks(&self, date: NaiveDate) -> Result<Vec<MemoryChunk>> {
        let state = self.state.lock().await;
        let mut out: Vec<MemoryChunk> = state
            .chunks
            .values()
            .filter(|c| c.created_at.date_naive() == date && c.kind != ChunkKind::Rollup)
            .cloned()
            .collect();
        out.sort_by_key(|c| c.created_at);
        Ok(out)
    }

    async fn fetch_rule_week_chunks(
        &self,
        rule_id: &str,
        week_start: NaiveDate,
    ) -> Result<Vec<MemoryChunk>> {
        let week_end = week_start
            .checked_add_days(chrono::Days::new(7))
            .ok_or_else(|| MemoryError::InvalidArgument(format!("week overflow: {week_start}")))?;
        let state = self.state.lock().await;
        let mut out: Vec<MemoryChunk> = state
            .chunks
            .values()
            .filter(|c| {
                c.kind == ChunkKind::Fire
                    && c.created_at.date_naive() >= week_start
                    && c.created_at.date_naive() < week_end
                    && Self::metadata_str(c, "rule_id").as_deref() == Some(rule_id)
            })
            .cloned()
            .collect();
        out.sort_by_key(|c| c.created_at);
        Ok(out)
    }

    async fn day_rollup_exists(&self, date: NaiveDate) -> Result<bool> {
        let date_s = date.to_string();
        let state = self.state.lock().await;
        Ok(state.chunks.values().any(|c| {
            c.kind == ChunkKind::Rollup
                && Self::metadata_str(c, "rollup_date") == Some(date_s.clone())
        }))
    }

    async fn rule_week_rollup_exists(&self, rule_id: &str, week_start: NaiveDate) -> Result<bool> {
        let week_s = week_start.to_string();
        let state = self.state.lock().await;
        Ok(state.chunks.values().any(|c| {
            c.kind == ChunkKind::Rollup
                && Self::metadata_str(c, "rollup_rule_id").as_deref() == Some(rule_id)
                && Self::metadata_str(c, "rollup_week_start") == Some(week_s.clone())
        }))
    }

    async fn link_summary_members(&self, rollup_id: &str, member_ids: &[String]) -> Result<()> {
        let mut state = self.state.lock().await;
        let entry = state
            .summary_members
            .entry(rollup_id.to_string())
            .or_default();
        for mid in member_ids {
            if !entry.contains(mid) {
                entry.push(mid.clone());
            }
        }
        Ok(())
    }

    fn map_summarizer_error(summarizer: &dyn Summarizer, err: SummarizerError) -> MemoryError {
        match err {
            SummarizerError::NoInput => {
                MemoryError::InvalidArgument("summarizer received no input".into())
            }
            other => {
                MemoryError::Provider(format!("summarizer {} failed: {other}", summarizer.name()))
            }
        }
    }

    async fn rollup_day_inner(&self, date: NaiveDate, force: bool) -> Result<Vec<MemoryChunk>> {
        let summarizer = self.summarizer.clone().ok_or(MemoryError::NotImplemented(
            "BasicMemoryProvider::rollup_day — no summarizer attached (call `with_summarizer` first)",
        ))?;

        if !force && self.day_rollup_exists(date).await? {
            return Ok(Vec::new());
        }

        let members = self.fetch_day_chunks(date).await?;
        if members.is_empty() {
            return Ok(Vec::new());
        }
        let member_ids: Vec<String> = members.iter().map(|c| c.id.clone()).collect();
        let ctx = SummaryContext {
            kind_label: Some(format!("day {date}")),
            note: Some(format!(
                "Daily rollup for {date} ({} chunks)",
                members.len()
            )),
            max_words: None,
        };
        let summary_text = summarizer
            .summarize(&members, &ctx)
            .await
            .map_err(|e| Self::map_summarizer_error(summarizer.as_ref(), e))?;

        let written = self
            .write(NewMemoryChunk {
                kind: ChunkKind::Rollup,
                source: ChunkSource::System,
                session_id: None,
                project_root: None,
                caller_id: "system".into(),
                content: summary_text,
                metadata: serde_json::json!({
                    "rollup_kind": "day",
                    "rollup_date": date.to_string(),
                    "member_count": member_ids.len(),
                    "summarizer": summarizer.name(),
                }),
                importance: None,
                shareable: false,
                pinned: false,
            })
            .await?;
        self.link_summary_members(&written.id, &member_ids).await?;
        Ok(vec![written])
    }

    async fn rollup_rule_week_inner(
        &self,
        rule_id: &str,
        week_start: NaiveDate,
        force: bool,
    ) -> Result<MemoryChunk> {
        let summarizer = self.summarizer.clone().ok_or(MemoryError::NotImplemented(
            "BasicMemoryProvider::rollup_rule_week — no summarizer attached \
             (call `with_summarizer` first)",
        ))?;

        if !force && self.rule_week_rollup_exists(rule_id, week_start).await? {
            return Err(MemoryError::InvalidArgument(format!(
                "rollup already exists for rule {rule_id} week {week_start}"
            )));
        }

        let members = self.fetch_rule_week_chunks(rule_id, week_start).await?;
        if members.is_empty() {
            return Err(MemoryError::NotFound(format!(
                "no fires for rule {rule_id} in week of {week_start}"
            )));
        }
        let member_ids: Vec<String> = members.iter().map(|c| c.id.clone()).collect();
        let ctx = SummaryContext {
            kind_label: Some(format!("week of {week_start} for rule {rule_id}")),
            note: Some(format!(
                "Weekly rollup for rule {rule_id} ({} fires)",
                members.len()
            )),
            max_words: None,
        };
        let summary_text = summarizer
            .summarize(&members, &ctx)
            .await
            .map_err(|e| Self::map_summarizer_error(summarizer.as_ref(), e))?;

        let written = self
            .write(NewMemoryChunk {
                kind: ChunkKind::Rollup,
                source: ChunkSource::System,
                session_id: None,
                project_root: None,
                caller_id: "system".into(),
                content: summary_text,
                metadata: serde_json::json!({
                    "rollup_kind": "rule_week",
                    "rollup_rule_id": rule_id,
                    "rollup_week_start": week_start.to_string(),
                    "member_count": member_ids.len(),
                    "summarizer": summarizer.name(),
                }),
                importance: None,
                shareable: false,
                pinned: false,
            })
            .await?;
        self.link_summary_members(&written.id, &member_ids).await?;
        Ok(written)
    }
}

#[async_trait]
impl MemoryProvider for BasicMemoryProvider {
    // ─────────────── Reads ───────────────

    async fn retrieve(&self, query: MemoryQuery) -> Result<Vec<MemoryChunk>> {
        if query.text.trim().is_empty() {
            return Err(MemoryError::InvalidArgument(
                "query.text must not be empty".into(),
            ));
        }
        let needle = query.text.to_lowercase();
        let state = self.state.lock().await;

        let mut hits: Vec<&MemoryChunk> = state
            .chunks
            .values()
            .filter(|c| {
                // Kind filter.
                if let Some(kinds) = &query.kinds {
                    if !kinds.contains(&c.kind) {
                        return false;
                    }
                }
                // include_rollups=false suppresses rollup chunks.
                if !query.include_rollups && c.kind == ChunkKind::Rollup {
                    return false;
                }
                // Time bounds.
                if let Some(since) = query.since {
                    if c.created_at < since {
                        return false;
                    }
                }
                if let Some(until) = query.until {
                    if c.created_at > until {
                        return false;
                    }
                }
                // Session restriction.
                if let Some(sid) = &query.session_id {
                    if c.session_id.as_deref() != Some(sid.as_str()) {
                        return false;
                    }
                }
                // Project root prefix.
                if let Some(prefix) = &query.project_root_prefix {
                    match &c.project_root {
                        Some(root) if root.starts_with(prefix.as_str()) => {}
                        _ => return false,
                    }
                }
                // Min importance.
                if let Some(min) = query.min_importance {
                    if c.importance < min {
                        return false;
                    }
                }
                // CallerScope enforcement. `Own` restricts to the caller's
                // own chunks. `OwnPlusShared` permits the caller's own
                // chunks *and* any chunk tagged `shareable=true` (the
                // Phase 4 multi-agent surface).
                // `Global` permits everything.
                match query.caller_scope {
                    crate::query::CallerScope::Own => {
                        if c.caller_id != query.caller_id {
                            return false;
                        }
                    }
                    crate::query::CallerScope::OwnPlusShared => {
                        if c.caller_id != query.caller_id && !c.shareable {
                            return false;
                        }
                    }
                    crate::query::CallerScope::Global => {}
                }
                // Lexical: substring of content (case-insensitive).
                c.content.to_lowercase().contains(&needle)
            })
            .collect();

        // Order by recency (newest first). The v1 stub does not perform
        // hybrid scoring — `RetrievalProfile` is accepted but ignored.
        hits.sort_by_key(|c| std::cmp::Reverse(c.created_at));
        hits.truncate(query.k);
        Ok(hits.into_iter().cloned().collect())
    }

    async fn get(&self, chunk_id: &str) -> Result<Option<MemoryChunk>> {
        let state = self.state.lock().await;
        Ok(state.chunks.get(chunk_id).cloned())
    }

    async fn get_session(&self, session_id: &str) -> Result<Option<MemorySession>> {
        let state = self.state.lock().await;
        Ok(state.sessions.get(session_id).cloned())
    }

    async fn list_sessions(&self, filter: SessionFilter) -> Result<Vec<MemorySession>> {
        let state = self.state.lock().await;
        let mut out: Vec<MemorySession> = state
            .sessions
            .values()
            .filter(|s| {
                if let Some(c) = &filter.caller_id {
                    if &s.caller_id != c {
                        return false;
                    }
                }
                if let Some(o) = filter.outcome {
                    if s.outcome != o {
                        return false;
                    }
                }
                if filter.open_only && s.outcome != SessionOutcome::Open {
                    return false;
                }
                if let Some(since) = filter.since {
                    if s.started_at < since {
                        return false;
                    }
                }
                if let Some(until) = filter.until {
                    if s.started_at > until {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        if let Some(n) = filter.limit {
            out.truncate(n);
        }
        Ok(out)
    }

    // ─────────────── Writes ───────────────

    async fn write(&self, new_chunk: NewMemoryChunk) -> Result<MemoryChunk> {
        if new_chunk.content.trim().is_empty() {
            return Err(MemoryError::InvalidArgument(
                "content must not be empty".into(),
            ));
        }

        // Consult the optional pre-write hook (memory_write_attempted seam).
        // On Redact, return a synthetic chunk with a `redacted: true` marker
        // and DO NOT persist it. Callers that care can inspect the returned
        // chunk; most won't notice the difference.
        let importance = crate::importance::score(&new_chunk);
        if let Some(hook) = &self.write_hook {
            match hook.before_write(&new_chunk).await? {
                crate::WriteDecision::Allow => {}
                crate::WriteDecision::Redact { reason } => {
                    return Ok(MemoryChunk {
                        id: Self::next_id(),
                        created_at: Utc::now(),
                        kind: new_chunk.kind,
                        tier: MemoryTier::Session,
                        source: new_chunk.source,
                        session_id: new_chunk.session_id,
                        project_root: new_chunk.project_root,
                        caller_id: new_chunk.caller_id,
                        content: format!("<redacted: {reason}>"),
                        metadata: serde_json::json!({"redacted": true, "reason": reason}),
                        importance: 0.0,
                        pinned: false,
                        shareable: false,
                        superseded_by: None,
                        embedding_model: "none".into(),
                        embedding_dim: 0,
                    });
                }
            }
        }

        let chunk = MemoryChunk {
            id: Self::next_id(),
            created_at: Utc::now(),
            kind: new_chunk.kind,
            tier: MemoryTier::Session,
            source: new_chunk.source,
            session_id: new_chunk.session_id,
            project_root: new_chunk.project_root,
            caller_id: new_chunk.caller_id,
            content: new_chunk.content,
            metadata: new_chunk.metadata,
            importance,
            pinned: new_chunk.pinned,
            shareable: new_chunk.shareable,
            superseded_by: None,
            embedding_model: "none".into(),
            embedding_dim: 0,
        };
        let mut state = self.state.lock().await;
        state.chunks.insert(chunk.id.clone(), chunk.clone());
        Ok(chunk)
    }

    async fn write_batch(&self, chunks: Vec<NewMemoryChunk>) -> Result<Vec<MemoryChunk>> {
        // The v1 stub doesn't batch embeddings (there are none). Process
        // one-by-one to keep the impl honest.
        let mut out = Vec::with_capacity(chunks.len());
        for nc in chunks {
            out.push(self.write(nc).await?);
        }
        Ok(out)
    }

    async fn open_session(&self, init: NewMemorySession) -> Result<MemorySession> {
        let s = MemorySession {
            id: Self::next_id(),
            started_at: Utc::now(),
            ended_at: None,
            caller_id: init.caller_id,
            title: init.title,
            summary: None,
            outcome: SessionOutcome::Open,
            metadata: init.metadata,
        };
        let mut state = self.state.lock().await;
        state.sessions.insert(s.id.clone(), s.clone());
        Ok(s)
    }

    async fn close_session(&self, session_id: &str, outcome: SessionOutcome) -> Result<()> {
        let mut state = self.state.lock().await;
        let s = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| MemoryError::NotFound(format!("session {session_id}")))?;
        s.ended_at = Some(Utc::now());
        s.outcome = match outcome {
            // Refuse to close into `Open`.
            SessionOutcome::Open => SessionOutcome::Aborted,
            other => other,
        };
        Ok(())
    }

    async fn rename_session(&self, session_id: &str, title: &str) -> Result<()> {
        let mut state = self.state.lock().await;
        let s = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| MemoryError::NotFound(format!("session {session_id}")))?;
        s.title = Some(title.to_string());
        Ok(())
    }

    // ─────────────── Updates ───────────────

    async fn pin(&self, chunk_id: &str, pinned: bool) -> Result<()> {
        let mut state = self.state.lock().await;
        let c = state
            .chunks
            .get_mut(chunk_id)
            .ok_or_else(|| MemoryError::NotFound(format!("chunk {chunk_id}")))?;
        c.pinned = pinned;
        Ok(())
    }

    async fn update_importance(&self, _chunk_id: &str, _importance: f32) -> Result<()> {
        // v1 does not score importance. No-op rather than NotImplemented:
        // callers can fire-and-forget without conditional logic.
        Ok(())
    }

    async fn supersede(&self, _old_id: &str, _new_id: &str) -> Result<()> {
        // v1 does not track supersession. No-op (callers may set it via the
        // full impl later; safe to call from any v1 caller).
        Ok(())
    }

    async fn record_access(&self, chunk_id: &str, retrieved_by: &str, used: bool) -> Result<()> {
        let mut state = self.state.lock().await;
        if !state.chunks.contains_key(chunk_id) {
            return Err(MemoryError::NotFound(format!("chunk {chunk_id}")));
        }
        state.accesses.push(AccessEntry {
            ts: Utc::now(),
            chunk_id: chunk_id.to_string(),
            retrieved_by: retrieved_by.to_string(),
            query_hash: String::new(), // v1 stub doesn't compute query hashes
            rank: 0,
            used,
        });
        Ok(())
    }

    // ─────────────── Deletes ───────────────

    async fn delete(&self, chunk_id: &str, reason: EvictionReason) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.chunks.remove(chunk_id).is_none() {
            return Err(MemoryError::NotFound(format!("chunk {chunk_id}")));
        }
        state.evictions.push(EvictionEntry {
            ts: Utc::now(),
            chunk_id: chunk_id.to_string(),
            reason,
            metadata: serde_json::Value::Null,
        });
        Ok(())
    }

    async fn delete_matching(
        &self,
        predicate: MemoryPredicate,
        reason: EvictionReason,
    ) -> Result<usize> {
        // Footgun guard: an empty predicate would match every chunk; callers
        // who want "delete everything" must use `purge_all`.
        if predicate.is_empty() {
            return Ok(0);
        }
        let mut state = self.state.lock().await;
        let now = Utc::now();
        let to_delete: Vec<String> = state
            .chunks
            .values()
            .filter(|c| chunk_matches(c, &predicate))
            .map(|c| c.id.clone())
            .collect();
        let count = to_delete.len();
        for id in to_delete {
            state.chunks.remove(&id);
            state.evictions.push(EvictionEntry {
                ts: now,
                chunk_id: id,
                reason,
                metadata: serde_json::Value::Null,
            });
        }
        Ok(count)
    }

    async fn purge_all(&self) -> Result<PurgeReport> {
        let mut state = self.state.lock().await;
        let report = PurgeReport {
            chunks_deleted: state.chunks.len(),
            sessions_deleted: state.sessions.len(),
            access_log_deleted: state.accesses.len(),
            eviction_log_deleted: state.evictions.len(),
        };
        state.chunks.clear();
        state.sessions.clear();
        state.accesses.clear();
        state.evictions.clear();
        state.summary_members.clear();
        Ok(report)
    }

    // ─────────────── Summarization ───────────────

    async fn summarize_session(&self, session_id: &str) -> Result<MemoryChunk> {
        let summarizer = self.summarizer.clone().ok_or(MemoryError::NotImplemented(
            "BasicMemoryProvider::summarize_session — no summarizer attached \
             (call `with_summarizer` first)",
        ))?;

        let session = self
            .get_session(session_id)
            .await?
            .ok_or_else(|| MemoryError::NotFound(format!("session {session_id}")))?;

        let members = self.fetch_session_chunks(session_id).await?;
        if members.is_empty() {
            return Err(MemoryError::InvalidArgument(format!(
                "session {session_id} has no chunks to summarize"
            )));
        }
        let member_ids: Vec<String> = members.iter().map(|c| c.id.clone()).collect();
        let ctx = SummaryContext {
            kind_label: Some("session".into()),
            note: session
                .title
                .as_ref()
                .map(|t| format!("session title: {t}")),
            max_words: None,
        };
        let summary_text = summarizer
            .summarize(&members, &ctx)
            .await
            .map_err(|e| Self::map_summarizer_error(summarizer.as_ref(), e))?;

        let written = self
            .write(NewMemoryChunk {
                kind: ChunkKind::JobSummary,
                source: ChunkSource::Embedded,
                session_id: Some(session_id.to_string()),
                project_root: members.iter().find_map(|c| c.project_root.clone()),
                caller_id: session.caller_id.clone(),
                content: summary_text.clone(),
                metadata: serde_json::json!({
                    "session_id": session_id,
                    "member_count": member_ids.len(),
                    "summarizer": summarizer.name(),
                }),
                importance: None,
                shareable: false,
                pinned: false,
            })
            .await?;
        self.link_summary_members(&written.id, &member_ids).await?;

        if let Some(session) = self.state.lock().await.sessions.get_mut(session_id) {
            session.summary = Some(summary_text);
        }

        Ok(written)
    }

    async fn rollup_day(&self, date: NaiveDate) -> Result<Vec<MemoryChunk>> {
        self.rollup_day_inner(date, false).await
    }

    async fn rollup_day_forced(&self, date: NaiveDate) -> Result<Vec<MemoryChunk>> {
        self.rollup_day_inner(date, true).await
    }

    async fn rollup_rule_week(&self, rule_id: &str, week_start: NaiveDate) -> Result<MemoryChunk> {
        self.rollup_rule_week_inner(rule_id, week_start, false)
            .await
    }

    async fn rollup_rule_week_forced(
        &self,
        rule_id: &str,
        week_start: NaiveDate,
    ) -> Result<MemoryChunk> {
        self.rollup_rule_week_inner(rule_id, week_start, true).await
    }

    // ─────────────── Maintenance ───────────────

    async fn run_aging_sweep(&self) -> Result<AgingReport> {
        // v1 stub: a 30-day retention sweep over non-pinned non-correction
        // chunks. No importance scoring; honest minimum.
        const RETENTION_DAYS: i64 = 30;
        let cutoff = Utc::now() - chrono::Duration::days(RETENTION_DAYS);
        let mut state = self.state.lock().await;
        let to_delete: Vec<String> = state
            .chunks
            .values()
            .filter(|c| !c.pinned && c.kind != ChunkKind::Correction && c.created_at < cutoff)
            .map(|c| c.id.clone())
            .collect();
        let deleted = to_delete.len();
        let now = Utc::now();
        for id in to_delete {
            state.chunks.remove(&id);
            state.evictions.push(EvictionEntry {
                ts: now,
                chunk_id: id,
                reason: EvictionReason::Aging,
                metadata: serde_json::Value::Null,
            });
        }
        Ok(AgingReport {
            tier_promoted: 0, // v1 doesn't transition tiers
            deleted,
            bytes_reclaimed: 0,
            deletions_by_reason: vec![(EvictionReason::Aging, deleted)],
        })
    }

    async fn re_embed_all(&self, target_model: &str) -> Result<ReEmbedReport> {
        let started = std::time::Instant::now();
        let mut state = self.state.lock().await;
        let total = state.chunks.len();
        for chunk in state.chunks.values_mut() {
            chunk.embedding_model = target_model.to_string();
        }
        Ok(ReEmbedReport {
            total,
            succeeded: total,
            failed: 0,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    async fn export(&self, filter: ExportFilter) -> Result<ExportBundle> {
        let state = self.state.lock().await;
        let chunks: Vec<MemoryChunk> = state
            .chunks
            .values()
            .filter(|c| match &filter.predicate {
                Some(p) if !p.is_empty() => chunk_matches(c, p),
                _ => true,
            })
            .cloned()
            .collect();

        let session_ids: std::collections::HashSet<String> =
            chunks.iter().filter_map(|c| c.session_id.clone()).collect();
        let sessions = if filter.include_sessions {
            state
                .sessions
                .values()
                .filter(|s| session_ids.contains(&s.id))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let evictions = if filter.include_eviction_log {
            state.evictions.clone()
        } else {
            Vec::new()
        };
        let accesses = if filter.include_access_log {
            state.accesses.clone()
        } else {
            Vec::new()
        };
        Ok(ExportBundle {
            chunks,
            sessions,
            evictions,
            accesses,
        })
    }

    async fn stats(&self) -> Result<MemoryStats> {
        let state = self.state.lock().await;
        let total = state.chunks.len();
        Ok(MemoryStats {
            total_chunks: total,
            session_chunks: state
                .chunks
                .values()
                .filter(|c| c.tier == MemoryTier::Session)
                .count(),
            long_term_chunks: state
                .chunks
                .values()
                .filter(|c| c.tier == MemoryTier::LongTerm)
                .count(),
            total_sessions: state.sessions.len(),
            open_sessions: state
                .sessions
                .values()
                .filter(|s| s.outcome == SessionOutcome::Open)
                .count(),
            db_bytes: 0, // in-memory: not meaningful in v1
            embedding_model: state
                .chunks
                .values()
                .find(|c| c.embedding_model != "none")
                .map(|c| c.embedding_model.clone()),
        })
    }
}

fn chunk_matches(c: &MemoryChunk, p: &MemoryPredicate) -> bool {
    if let Some(kinds) = &p.kinds {
        if !kinds.contains(&c.kind) {
            return false;
        }
    }
    if let Some(callers) = &p.callers {
        if !callers.iter().any(|x| x == &c.caller_id) {
            return false;
        }
    }
    if let Some(sids) = &p.session_ids {
        match &c.session_id {
            Some(id) if sids.iter().any(|x| x == id) => {}
            _ => return false,
        }
    }
    if let Some(prefix) = &p.project_root_prefix {
        match &c.project_root {
            Some(root) if root.starts_with(prefix.as_str()) => {}
            _ => return false,
        }
    }
    if let Some(before) = p.before {
        if c.created_at >= before {
            return false;
        }
    }
    if let Some(after) = p.after {
        if c.created_at <= after {
            return false;
        }
    }
    if let Some(pinned) = p.pinned {
        if c.pinned != pinned {
            return false;
        }
    }
    if let Some(below) = p.importance_below {
        if c.importance >= below {
            return false;
        }
    }
    if let Some(needle) = &p.content_contains {
        if !c.content.to_lowercase().contains(&needle.to_lowercase()) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::ChunkSource;
    use crate::query::{CallerScope, RetrievalProfile};
    use serde_json::json;

    fn nc(caller: &str, content: &str) -> NewMemoryChunk {
        NewMemoryChunk {
            kind: ChunkKind::Chat,
            source: ChunkSource::Embedded,
            session_id: None,
            project_root: None,
            caller_id: caller.into(),
            content: content.into(),
            metadata: json!(null),
            importance: None,
            shareable: false,
            pinned: false,
        }
    }

    fn q(caller: &str, text: &str) -> MemoryQuery {
        MemoryQuery {
            text: text.into(),
            kinds: None,
            since: None,
            until: None,
            session_id: None,
            caller_scope: CallerScope::Own,
            project_root_prefix: None,
            k: 8,
            include_rollups: true,
            min_importance: None,
            profile: RetrievalProfile::AgentChatTurn,
            caller_id: caller.into(),
        }
    }

    #[tokio::test]
    async fn write_and_retrieve_round_trips() {
        let m = BasicMemoryProvider::new();
        let c = m
            .write(nc("embedded", "Q4 report is filed under Workspace"))
            .await
            .unwrap();
        let hits = m.retrieve(q("embedded", "q4 report")).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, c.id);
    }

    #[tokio::test]
    async fn retrieve_respects_caller_scope_own() {
        let m = BasicMemoryProvider::new();
        m.write(nc("embedded", "my secret")).await.unwrap();
        m.write(nc("mcp:cursor", "my secret")).await.unwrap();
        let hits = m.retrieve(q("embedded", "secret")).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].caller_id, "embedded");
    }

    #[tokio::test]
    async fn retrieve_global_scope_sees_all() {
        let m = BasicMemoryProvider::new();
        m.write(nc("embedded", "my secret")).await.unwrap();
        m.write(nc("mcp:cursor", "my secret")).await.unwrap();
        let mut query = q("audit", "secret");
        query.caller_scope = CallerScope::Global;
        let hits = m.retrieve(query).await.unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[tokio::test]
    async fn retrieve_own_plus_shared_surfaces_shareable_across_callers() {
        let m = BasicMemoryProvider::new();
        // Two callers, two chunks: one shareable from "mcp:cursor",
        // one private from "mcp:cursor". A third caller asks with
        // OwnPlusShared scope; only the shareable one surfaces.
        let mut shared = nc("mcp:cursor", "user prefers dry-run mode");
        shared.shareable = true;
        m.write(shared).await.unwrap();
        m.write(nc("mcp:cursor", "user said hi to cursor"))
            .await
            .unwrap();
        m.write(nc("mcp:codex", "user said hi to codex"))
            .await
            .unwrap();
        let mut query = q("embedded", "user");
        query.caller_scope = CallerScope::OwnPlusShared;
        let hits = m.retrieve(query).await.unwrap();
        // Only the shareable chunk is visible to the embedded caller.
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("dry-run"));
        assert!(hits[0].shareable);
    }

    #[tokio::test]
    async fn retrieve_own_plus_shared_includes_own_unshared() {
        let m = BasicMemoryProvider::new();
        // The caller's own chunks are always visible under OwnPlusShared
        // regardless of the shareable flag.
        m.write(nc("embedded", "embedded private note"))
            .await
            .unwrap();
        m.write(nc("mcp:cursor", "cursor private note"))
            .await
            .unwrap();
        let mut query = q("embedded", "note");
        query.caller_scope = CallerScope::OwnPlusShared;
        let hits = m.retrieve(query).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].caller_id, "embedded");
    }

    #[tokio::test]
    async fn retrieve_orders_recent_first() {
        let m = BasicMemoryProvider::new();
        m.write(nc("embedded", "first about cats")).await.unwrap();
        // Small delay to ensure created_at differs.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let second = m.write(nc("embedded", "second about cats")).await.unwrap();
        let hits = m.retrieve(q("embedded", "cats")).await.unwrap();
        assert_eq!(hits[0].id, second.id);
    }

    #[tokio::test]
    async fn retrieve_kind_filter() {
        let m = BasicMemoryProvider::new();
        let chat = m.write(nc("embedded", "alpha")).await.unwrap();
        let mut action = nc("embedded", "alpha");
        action.kind = ChunkKind::Action;
        let _ = m.write(action).await.unwrap();
        let mut query = q("embedded", "alpha");
        query.kinds = Some(vec![ChunkKind::Chat]);
        let hits = m.retrieve(query).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, chat.id);
    }

    #[tokio::test]
    async fn empty_query_text_rejected() {
        let m = BasicMemoryProvider::new();
        let err = m.retrieve(q("embedded", "   ")).await.unwrap_err();
        assert!(matches!(err, MemoryError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn empty_content_rejected() {
        let m = BasicMemoryProvider::new();
        let err = m.write(nc("embedded", "")).await.unwrap_err();
        assert!(matches!(err, MemoryError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn session_open_close() {
        let m = BasicMemoryProvider::new();
        let s = m
            .open_session(NewMemorySession {
                caller_id: "embedded".into(),
                title: Some("test".into()),
                metadata: json!(null),
            })
            .await
            .unwrap();
        assert_eq!(s.outcome, SessionOutcome::Open);
        m.close_session(&s.id, SessionOutcome::Success)
            .await
            .unwrap();
        let s2 = m.get_session(&s.id).await.unwrap().unwrap();
        assert_eq!(s2.outcome, SessionOutcome::Success);
        assert!(s2.ended_at.is_some());
    }

    #[tokio::test]
    async fn close_session_unknown_id_errors() {
        let m = BasicMemoryProvider::new();
        let err = m
            .close_session("nope", SessionOutcome::Success)
            .await
            .unwrap_err();
        assert!(matches!(err, MemoryError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_matching_empty_predicate_is_noop() {
        let m = BasicMemoryProvider::new();
        m.write(nc("embedded", "x")).await.unwrap();
        let n = m
            .delete_matching(MemoryPredicate::default(), EvictionReason::UserDelete)
            .await
            .unwrap();
        assert_eq!(n, 0);
        let stats = m.stats().await.unwrap();
        assert_eq!(stats.total_chunks, 1);
    }

    #[tokio::test]
    async fn delete_matching_by_caller() {
        let m = BasicMemoryProvider::new();
        m.write(nc("embedded", "x")).await.unwrap();
        m.write(nc("mcp:cursor", "y")).await.unwrap();
        let n = m
            .delete_matching(
                MemoryPredicate {
                    callers: Some(vec!["embedded".into()]),
                    ..Default::default()
                },
                EvictionReason::UserDelete,
            )
            .await
            .unwrap();
        assert_eq!(n, 1);
        let stats = m.stats().await.unwrap();
        assert_eq!(stats.total_chunks, 1);
    }

    #[tokio::test]
    async fn purge_all_clears_state() {
        let m = BasicMemoryProvider::new();
        m.write(nc("embedded", "x")).await.unwrap();
        let _ = m
            .open_session(NewMemorySession {
                caller_id: "embedded".into(),
                title: None,
                metadata: json!(null),
            })
            .await
            .unwrap();
        let r = m.purge_all().await.unwrap();
        assert_eq!(r.chunks_deleted, 1);
        assert_eq!(r.sessions_deleted, 1);
        let stats = m.stats().await.unwrap();
        assert_eq!(stats.total_chunks, 0);
        assert_eq!(stats.total_sessions, 0);
    }

    #[tokio::test]
    async fn summarize_without_summarizer_returns_not_implemented() {
        let m = BasicMemoryProvider::new();
        let err = m.summarize_session("anything").await.unwrap_err();
        assert!(matches!(err, MemoryError::NotImplemented(_)));
    }

    #[tokio::test]
    async fn summarize_session_with_summarizer_writes_summary() {
        let summarizer = crate::MockSummarizer::new("session recap");
        let m = BasicMemoryProvider::new().with_summarizer(summarizer);
        let session = m
            .open_session(NewMemorySession {
                caller_id: "embedded".into(),
                title: Some("chat".into()),
                metadata: json!(null),
            })
            .await
            .unwrap();
        m.write(NewMemoryChunk {
            kind: ChunkKind::Chat,
            source: ChunkSource::Embedded,
            caller_id: "embedded".into(),
            content: "hello".into(),
            session_id: Some(session.id.clone()),
            project_root: None,
            metadata: json!(null),
            importance: None,
            shareable: false,
            pinned: false,
        })
        .await
        .unwrap();

        let summary = m.summarize_session(&session.id).await.unwrap();
        assert_eq!(summary.kind, ChunkKind::JobSummary);
        assert_eq!(summary.content, "session recap");

        let updated = m.get_session(&session.id).await.unwrap().unwrap();
        assert_eq!(updated.summary.as_deref(), Some("session recap"));
    }

    #[tokio::test]
    async fn re_embed_updates_embedding_model_metadata() {
        let m = BasicMemoryProvider::new();
        let chunk = m.write(nc("embedded", "hello")).await.unwrap();
        assert_eq!(chunk.embedding_model, "none");

        let report = m.re_embed_all("mock-384").await.unwrap();
        assert_eq!(report.total, 1);
        assert_eq!(report.succeeded, 1);
        assert_eq!(report.failed, 0);

        let got = m.get(&chunk.id).await.unwrap().unwrap();
        assert_eq!(got.embedding_model, "mock-384");
    }

    #[tokio::test]
    async fn update_importance_and_supersede_noop() {
        let m = BasicMemoryProvider::new();
        let c = m.write(nc("embedded", "x")).await.unwrap();
        m.update_importance(&c.id, 0.9).await.unwrap();
        m.supersede(&c.id, "other").await.unwrap();
        // Importance unchanged: v1 stub no-ops.
        let got = m.get(&c.id).await.unwrap().unwrap();
        assert_eq!(got.importance, 0.5);
        assert!(got.superseded_by.is_none());
    }

    #[tokio::test]
    async fn export_filters_by_predicate() {
        let m = BasicMemoryProvider::new();
        m.write(nc("embedded", "alpha")).await.unwrap();
        m.write(nc("embedded", "beta")).await.unwrap();
        let bundle = m
            .export(ExportFilter {
                predicate: Some(MemoryPredicate {
                    content_contains: Some("alpha".into()),
                    ..Default::default()
                }),
                include_eviction_log: false,
                include_access_log: false,
                include_sessions: true,
            })
            .await
            .unwrap();
        assert_eq!(bundle.chunks.len(), 1);
        assert!(bundle.chunks[0].content.contains("alpha"));
    }

    #[tokio::test]
    async fn aging_sweep_deletes_old_unpinned() {
        let m = BasicMemoryProvider::new();
        let fresh = m.write(nc("embedded", "fresh")).await.unwrap();
        // Forcibly age a chunk by editing the state directly.
        {
            let mut state = m.state.lock().await;
            let c = state.chunks.get_mut(&fresh.id).unwrap();
            c.created_at = Utc::now() - chrono::Duration::days(45);
        }
        let r = m.run_aging_sweep().await.unwrap();
        assert_eq!(r.deleted, 1);
        let stats = m.stats().await.unwrap();
        assert_eq!(stats.total_chunks, 0);
    }

    #[tokio::test]
    async fn aging_sweep_preserves_pinned_and_corrections() {
        let m = BasicMemoryProvider::new();
        let pinned = m.write(nc("embedded", "pinned")).await.unwrap();
        let mut correction_nc = nc("embedded", "correction");
        correction_nc.kind = ChunkKind::Correction;
        let correction = m.write(correction_nc).await.unwrap();
        {
            let mut state = m.state.lock().await;
            for id in [&pinned.id, &correction.id] {
                let c = state.chunks.get_mut(id).unwrap();
                c.created_at = Utc::now() - chrono::Duration::days(45);
            }
            // Pin the pinned one.
            let c = state.chunks.get_mut(&pinned.id).unwrap();
            c.pinned = true;
        }
        let r = m.run_aging_sweep().await.unwrap();
        assert_eq!(r.deleted, 0);
    }
}
