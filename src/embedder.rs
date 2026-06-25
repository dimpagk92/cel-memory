//! Embedder trait and reference implementations.
//!
//! The [`Embedder`] trait abstracts over local (fastembed/ONNX) and cloud
//! (OpenAI/Voyage) embedding backends. Storage backends take a
//! `Arc<dyn Embedder>` at construction and use it for write-time embedding
//! and retrieval-time query embedding.
//!
//! v1 ships [`MockEmbedder`] — deterministic small-dim vectors for tests.
//! Production embedders (e.g. `FastEmbedEmbedder` in `cel-memory-sqlite`)
//! live in backend crates.

use async_trait::async_trait;
use thiserror::Error;

/// Errors produced by an [`Embedder`].
///
/// Distinct from [`crate::MemoryError`] so the trait can be implemented in
/// backend crates without coupling to the full memory error surface. Callers
/// map these at the provider boundary via [`From<EmbedderError>`].
#[derive(Debug, Error)]
pub enum EmbedderError {
    /// The embedder's upstream client failed (model load, rate limit, etc.).
    #[error("provider error: {0}")]
    Provider(String),

    /// The embedder produced a vector with the wrong dimensionality.
    #[error("embedding dim mismatch: expected {expected}, got {actual}")]
    DimMismatch {
        /// Declared [`Embedder::dim`].
        expected: usize,
        /// Length of the vector actually returned.
        actual: usize,
    },

    /// An unexpected internal error. Indicates a bug.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Result alias for embedder operations.
pub type EmbedderResult<T> = std::result::Result<T, EmbedderError>;

/// An embedder turns text into a fixed-dimension vector.
///
/// Implementations must produce vectors of [`Embedder::dim`] length on
/// every call; the storage layer validates and rejects mismatches.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Vector dimensionality.
    fn dim(&self) -> usize;

    /// Stable model identifier (e.g., `"bge-small-en-v1.5"`).
    fn model_name(&self) -> &str;

    /// Embed one piece of text.
    async fn embed(&self, text: &str) -> EmbedderResult<Vec<f32>>;

    /// Embed a batch of texts. Default implementation calls [`embed`]
    /// sequentially; production embedders should override for batching.
    ///
    /// [`embed`]: Embedder::embed
    async fn embed_batch(&self, texts: &[String]) -> EmbedderResult<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }
}

/// Deterministic test embedder. Hashes the input text to a small vector
/// of pseudo-random floats. **Never use in production** — produces
/// meaningless vectors.
///
/// Useful for unit tests of storage backends where we just need *some*
/// vector to round-trip through vector tables.
#[derive(Debug, Clone)]
pub struct MockEmbedder {
    dim: usize,
    model: String,
}

impl MockEmbedder {
    /// Mock embedder with the default `384` dimension, matching common
    /// local embedding models.
    pub fn new() -> Self {
        Self {
            dim: 384,
            model: "mock-384".into(),
        }
    }

    /// Mock embedder with an arbitrary dim. Use only for tests that
    /// override the migration schema.
    pub fn with_dim(dim: usize) -> Self {
        Self {
            dim,
            model: format!("mock-{dim}"),
        }
    }
}

impl Default for MockEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Embedder for MockEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    async fn embed(&self, text: &str) -> EmbedderResult<Vec<f32>> {
        let mut seed: u64 = 0xcbf29ce484222325;
        for b in text.bytes() {
            seed ^= b as u64;
            seed = seed.wrapping_mul(0x100000001b3);
        }
        let mut out = Vec::with_capacity(self.dim);
        for i in 0..self.dim {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let f = ((seed >> (i % 32)) as i32) as f32 / i32::MAX as f32;
            out.push(f.clamp(-1.0, 1.0));
        }
        Ok(out)
    }
}

impl From<EmbedderError> for crate::MemoryError {
    fn from(e: EmbedderError) -> Self {
        match e {
            EmbedderError::DimMismatch { expected, actual } => crate::MemoryError::InvalidArgument(
                format!("embedding dim mismatch: expected {expected}, got {actual}"),
            ),
            EmbedderError::Provider(msg) => crate::MemoryError::Provider(msg),
            EmbedderError::Internal(msg) => crate::MemoryError::Internal(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_embedder_deterministic() {
        let e = MockEmbedder::new();
        let a = e.embed("hello").await.unwrap();
        let b = e.embed("hello").await.unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 384);
    }

    #[tokio::test]
    async fn mock_embedder_different_for_different_input() {
        let e = MockEmbedder::new();
        let a = e.embed("hello").await.unwrap();
        let b = e.embed("world").await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn mock_embedder_with_dim_honors_dim() {
        let e = MockEmbedder::with_dim(8);
        let v = e.embed("hi").await.unwrap();
        assert_eq!(v.len(), 8);
        assert_eq!(e.dim(), 8);
    }

    #[tokio::test]
    async fn batch_default_works() {
        let e = MockEmbedder::new();
        let v = e
            .embed_batch(&["a".to_string(), "b".to_string()])
            .await
            .unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].len(), 384);
    }
}
