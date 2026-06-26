//! Shared behavioral checks for [`MemoryProvider`] implementations.
//!
//! Backend crates should call these helpers from integration tests so every
//! persistence layer honors the same contract. See
//! [`cel-memory-sqlite/tests/swap.rs`](https://github.com/dimpagk92/cel-memory-sqlite/blob/main/tests/swap.rs)
//! for a downstream example.

use std::sync::Arc;

use crate::{
    CallerScope, ChunkKind, ChunkSource, MemoryChunk, MemoryProvider, MemoryQuery, MemorySession,
    MemoryStats, NewMemoryChunk, NewMemorySession, Result, RetrievalProfile, SessionOutcome,
};

/// Minimal contract every backend must satisfy: write a chunk, read it back,
/// and report coherent stats.
pub async fn assert_write_get_stats(
    memory: Arc<dyn MemoryProvider>,
    content: &str,
) -> Result<(MemoryChunk, MemoryStats)> {
    let chunk = memory
        .write(NewMemoryChunk {
            kind: ChunkKind::Chat,
            source: ChunkSource::Embedded,
            session_id: None,
            project_root: None,
            caller_id: "conformance".into(),
            content: content.into(),
            metadata: serde_json::Value::Null,
            importance: None,
            shareable: false,
            pinned: false,
        })
        .await?;

    let fetched = memory
        .get(&chunk.id)
        .await?
        .ok_or_else(|| crate::MemoryError::NotFound(chunk.id.clone()))?;
    assert_eq!(fetched.content, chunk.content);

    let stats = memory.stats().await?;
    assert!(stats.total_chunks >= 1);

    Ok((chunk, stats))
}

/// Hybrid retrieval must surface a chunk the provider just wrote.
pub async fn assert_retrieve_finds_written(
    memory: Arc<dyn MemoryProvider>,
    content: &str,
) -> Result<MemoryChunk> {
    let chunk = memory
        .write(NewMemoryChunk {
            kind: ChunkKind::Chat,
            source: ChunkSource::Embedded,
            session_id: None,
            project_root: None,
            caller_id: "conformance".into(),
            content: content.into(),
            metadata: serde_json::Value::Null,
            importance: None,
            shareable: false,
            pinned: false,
        })
        .await?;

    let hits = memory
        .retrieve(MemoryQuery {
            text: content.into(),
            kinds: Some(vec![ChunkKind::Chat]),
            since: None,
            until: None,
            session_id: None,
            caller_scope: CallerScope::Global,
            project_root_prefix: None,
            k: 8,
            include_rollups: true,
            min_importance: None,
            profile: RetrievalProfile::AgentChatTurn,
            caller_id: "conformance".into(),
        })
        .await?;

    assert!(
        hits.iter().any(|hit| hit.id == chunk.id),
        "retrieve did not return the written chunk; got {} hits",
        hits.len()
    );

    Ok(chunk)
}

/// Session open → scoped write → close must round-trip through `get_session`.
pub async fn assert_session_lifecycle(memory: Arc<dyn MemoryProvider>) -> Result<MemorySession> {
    let session = memory
        .open_session(NewMemorySession {
            caller_id: "conformance".into(),
            title: Some("conformance session".into()),
            metadata: serde_json::Value::Null,
        })
        .await?;

    memory
        .write(NewMemoryChunk {
            kind: ChunkKind::Chat,
            source: ChunkSource::Embedded,
            session_id: Some(session.id.clone()),
            project_root: None,
            caller_id: session.caller_id.clone(),
            content: "session-scoped turn".into(),
            metadata: serde_json::Value::Null,
            importance: None,
            shareable: false,
            pinned: false,
        })
        .await?;

    let open = memory
        .get_session(&session.id)
        .await?
        .ok_or_else(|| crate::MemoryError::NotFound(session.id.clone()))?;
    assert_eq!(open.outcome, SessionOutcome::Open);

    memory
        .close_session(&session.id, SessionOutcome::Success)
        .await?;

    let closed = memory
        .get_session(&session.id)
        .await?
        .ok_or_else(|| crate::MemoryError::NotFound(session.id.clone()))?;
    assert_eq!(closed.outcome, SessionOutcome::Success);

    Ok(closed)
}

/// Summarization must produce a persisted `JobSummary` linked to the session.
///
/// Backends without summarization support should skip calling this helper.
pub async fn assert_summarize_session_roundtrip(
    memory: Arc<dyn MemoryProvider>,
) -> Result<MemoryChunk> {
    let session = memory
        .open_session(NewMemorySession {
            caller_id: "conformance".into(),
            title: Some("summarize me".into()),
            metadata: serde_json::Value::Null,
        })
        .await?;

    memory
        .write(NewMemoryChunk {
            kind: ChunkKind::Chat,
            source: ChunkSource::Embedded,
            session_id: Some(session.id.clone()),
            project_root: None,
            caller_id: session.caller_id.clone(),
            content: "first turn in session".into(),
            metadata: serde_json::Value::Null,
            importance: None,
            shareable: false,
            pinned: false,
        })
        .await?;
    memory
        .write(NewMemoryChunk {
            kind: ChunkKind::Chat,
            source: ChunkSource::Embedded,
            session_id: Some(session.id.clone()),
            project_root: None,
            caller_id: session.caller_id.clone(),
            content: "second turn in session".into(),
            metadata: serde_json::Value::Null,
            importance: None,
            shareable: false,
            pinned: false,
        })
        .await?;

    let summary = memory.summarize_session(&session.id).await?;
    assert_eq!(summary.kind, ChunkKind::JobSummary);
    assert!(
        memory.get(&summary.id).await?.is_some(),
        "summary chunk was not persisted"
    );

    Ok(summary)
}
