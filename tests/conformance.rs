//! Conformance checks for the in-crate reference provider.

use std::sync::Arc;

use cel_memory::{
    assert_retrieve_finds_written, assert_session_lifecycle, assert_write_get_stats,
    BasicMemoryProvider, MemoryProvider,
};

#[tokio::test]
async fn basic_provider_passes_conformance() {
    let memory: Arc<dyn MemoryProvider> = Arc::new(BasicMemoryProvider::new());
    let (_chunk, stats) = assert_write_get_stats(memory.clone(), "conformance probe")
        .await
        .unwrap();
    assert_eq!(stats.total_chunks, 1);

    assert_retrieve_finds_written(memory.clone(), "unique retrieval phrase 7f3a")
        .await
        .unwrap();
    assert_session_lifecycle(memory).await.unwrap();
}
