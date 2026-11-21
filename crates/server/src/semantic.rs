//! Semantic search over precomputed chunk embeddings.
//!
//! Data file: `$SHUKU_DATA_DIR/semantic.json` (generated offline by the
//! pilot tooling from the 6 most-recently-read books). If the file is
//! absent, semantic search is unavailable and the API reports
//! `available: false`; the rest of the service is unaffected.
//!
//! Vectors are int8-quantized per chunk (a `min` and a `scale` per chunk
//! are stored alongside); cosine similarity is computed on the
//! dequantized float values. Quantization was verified to leave the
//! pilot rankings unchanged (1, 6, 1, 1 out of 14235 chunks, identical
//! scores to within 0.001).

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Deserialize)]
pub struct SemanticChunk {
    pub book_id: String,
    pub book_title: String,
    pub chapter_idx: i64,
    pub chapter_title: String,
    pub chunk_idx: i64,
    /// Display snippet (truncated to ~250 chars at generation time).
    pub text: String,
}

#[derive(Debug, Deserialize)]
struct SemanticFile {
    dims: usize,
    chunks: Vec<SemanticChunk>,
    vectors_q_b64: String,
    mins: Vec<f32>,
    scales: Vec<f32>,
}

pub struct SemanticStore {
    pub dims: usize,
    pub chunks: Vec<SemanticChunk>,
    /// Dequantized float vectors, row-major [count, dims].
    vectors: Vec<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SemanticHit {
    pub book_id: String,
    pub book_title: String,
    pub chapter_idx: i64,
    pub chapter_title: String,
    pub chunk_idx: i64,
    pub snippet: String,
    pub score: f32,
}

impl SemanticStore {
    /// Load and dequantize the store from `path`. Returns an error when
    /// the file is missing or malformed; the caller decides whether that
    /// means "unavailable" (startup) or a hard failure.
    pub fn load(path: &Path) -> Result<Arc<Self>> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read semantic store {}", path.display()))?;
        let file: SemanticFile =
            serde_json::from_str(&text).context("parse semantic.json")?;
        let count = file.chunks.len();
        if count == 0 {
            bail!("semantic.json: no chunks");
        }
        if file.dims == 0 {
            bail!("semantic.json: dims is 0");
        }
        if file.mins.len() != count || file.scales.len() != count {
            bail!("semantic.json: mins/scales length mismatch");
        }
        let qbytes = B64
            .decode(&file.vectors_q_b64)
            .context("decode vectors_q_b64")?;
        if qbytes.len() != count * file.dims {
            bail!("semantic.json: vector bytes length mismatch");
        }
        let mut vectors = vec![0f32; count * file.dims];
        for i in 0..count {
            let mn = file.mins[i];
            let sc = file.scales[i];
            let base = i * file.dims;
            for j in 0..file.dims {
                vectors[base + j] = mn + qbytes[base + j] as f32 * sc;
            }
        }
        Ok(Arc::new(Self {
            dims: file.dims,
            chunks: file.chunks,
            vectors,
        }))
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    pub fn book_count(&self) -> usize {
        let mut ids: HashSet<&str> = HashSet::new();
        for c in &self.chunks {
            ids.insert(c.book_id.as_str());
        }
        ids.len()
    }

    /// Top-`top_k` chunks by cosine similarity to `query`. The query is
    /// normalized defensively (the browser sends a normalized vector, but
    /// a caller that does not must not skew results).
    pub fn search(&self, query: &[f32], top_k: usize) -> Vec<SemanticHit> {
        if query.len() != self.dims {
            return Vec::new();
        }
        let qn = normalize(query);
        let count = self.chunks.len();
        let mut scored: Vec<(usize, f32)> = Vec::with_capacity(count);
        for i in 0..count {
            let v = &self.vectors[i * self.dims..(i + 1) * self.dims];
            scored.push((i, cosine(&qn, v)));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k.clamp(1, 50));
        scored
            .into_iter()
            .map(|(i, s)| {
                let c = &self.chunks[i];
                SemanticHit {
                    book_id: c.book_id.clone(),
                    book_title: c.book_title.clone(),
                    chapter_idx: c.chapter_idx,
                    chapter_title: c.chapter_title.clone(),
                    chunk_idx: c.chunk_idx,
                    snippet: c.text.clone(),
                    score: s,
                }
            })
            .collect()
    }
}

fn normalize(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n == 0.0 || !n.is_finite() {
        return v.to_vec();
    }
    v.iter().map(|x| x / n).collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    dot / (na.sqrt() * nb.sqrt() + 1e-9)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_store() -> SemanticStore {
        // 3 chunks, dims 2: chunk 0 = (1,0), chunk 1 = (0,1), chunk 2 = (1,1)/sqrt2.
        let chunks = vec![
            SemanticChunk { book_id: "b1".into(), book_title: "T1".into(), chapter_idx: 0, chapter_title: "C1".into(), chunk_idx: 0, text: "x".into() },
            SemanticChunk { book_id: "b1".into(), book_title: "T1".into(), chapter_idx: 0, chapter_title: "C1".into(), chunk_idx: 1, text: "y".into() },
            SemanticChunk { book_id: "b2".into(), book_title: "T2".into(), chapter_idx: 1, chapter_title: "C2".into(), chunk_idx: 0, text: "z".into() },
        ];
        let vectors = vec![1.0, 0.0, 0.0, 1.0, 0.7071, 0.7071];
        SemanticStore { dims: 2, chunks, vectors }
    }

    #[test]
    fn search_ranks_by_cosine() {
        let s = tiny_store();
        let hits = s.search(&[1.0, 0.0], 3);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].chunk_idx, 0);
        assert_eq!(hits[1].chunk_idx, 0);
        assert!(hits[1].book_id == "b2");
        assert_eq!(hits[2].chunk_idx, 1);
    }

    #[test]
    fn search_rejects_wrong_dims() {
        let s = tiny_store();
        assert!(s.search(&[1.0, 0.0, 0.0], 3).is_empty());
    }

    #[test]
    fn load_roundtrip_via_json() {
        // Build a minimal semantic.json in the same shape the generator
        // writes, load it, and check the dequantized search result.
        let file = serde_json::json!({
            "dims": 2,
            "chunks": [
                {"book_id": "b1", "book_title": "T1", "chapter_idx": 0, "chapter_title": "C1", "chunk_idx": 0, "text": "x"},
                {"book_id": "b1", "book_title": "T1", "chapter_idx": 0, "chapter_title": "C1", "chunk_idx": 1, "text": "y"},
            ],
            // chunk0 (1,0) -> min 0 scale 1/255; chunk1 (0,1) -> min 0 scale 1/255.
            "vectors_q_b64": B64.encode([255u8, 0, 0, 255]),
            "mins": [0.0, 0.0],
            "scales": [1.0 / 255.0, 1.0 / 255.0],
        });
        let dir = std::env::temp_dir().join(format!("shuku-sem-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("semantic.json");
        std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();
        let store = SemanticStore::load(&path).unwrap();
        assert_eq!(store.chunk_count(), 2);
        assert_eq!(store.book_count(), 1);
        let hits = store.search(&[1.0, 0.0], 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_idx, 0);
        assert!(hits[0].score > 0.99);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
