//! Tiny deterministic text embedder (demo only) — a 16-dim bag-of-tokens,
//! L2-normalised. Lexically-similar text → similar vectors, so vector search
//! is demonstrable WITHOUT a real embedding model. The SAME algorithm is
//! reimplemented in agent-memory.html (JS) so browser queries match the stored
//! vectors exactly. Swap this for a real embedder (e.g. an MCP tool that calls
//! an embedding API) for production semantics.

pub const DIM: usize = 16;

/// 16-dim L2-normalised bag-of-tokens embedding of `text`.
/// Tokens = maximal `[a-z0-9]` runs of the lowercased text; each token's byte
/// sum mod 16 picks a dimension to increment.
pub fn embed16(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; DIM];
    for tok in text
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let s: u32 = tok.bytes().map(u32::from).sum();
        v[(s % DIM as u32) as usize] += 1.0;
    }
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        for x in &mut v {
            *x /= n;
        }
    }
    v
}

/// Cosine similarity of two equal-length vectors (both assumed L2-normalised,
/// so this is just the dot product).
#[allow(dead_code)]
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
