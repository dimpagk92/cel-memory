//! Conformance checks for the in-crate reference provider.

use std::sync::Arc;

use cel_memory::{assert_write_get_stats, BasicMemoryProvider, MemoryProvider};

#[tokio::test]
async fn basic_provider_passes_conformance() {
    let memory: Arc<dyn MemoryProvider> = Arc::new(BasicMemoryProvider::new());
    let (_chunk, stats) = assert_write_get_stats(memory, "conformance probe")
        .await
        .unwrap();
    assert_eq!(stats.total_chunks, 1);
}
