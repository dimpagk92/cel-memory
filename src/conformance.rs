//! Shared behavioral checks for [`MemoryProvider`] implementations.
//!
//! Backend crates should call these helpers from integration tests so every
//! persistence layer honors the same contract. See
//! [`cel-memory-sqlite/tests/swap.rs`](https://github.com/dimpagk92/cel-memory-sqlite/blob/main/tests/swap.rs)
//! for a downstream example.

use std::sync::Arc;

use crate::{
    ChunkKind, ChunkSource, MemoryChunk, MemoryProvider, MemoryStats, NewMemoryChunk, Result,
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
