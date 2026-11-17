use std::collections::HashMap;
use std::hash::Hash;

pub const COS_FLOOR: f32 = 0.53;
pub const RRF_K: f64 = 60.0;

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// Computes Reciprocal Rank Fusion (RRF) over FTS5 BM25 ranked IDs and vector similarity ranked IDs.
/// `fts_ranked`: ordered from best (rank 1) to worst.
/// `vec_scored`: `(id, cosine_sim)` pairs; only items with `cosine_sim >= COS_FLOOR` (0.53) are accepted.
pub fn reciprocal_rank_fusion<K: Eq + Hash + Clone>(
    fts_ranked: &[K],
    mut vec_scored: Vec<(K, f32)>,
) -> Vec<(K, f64)> {
    // Filter by cos_floor = 0.53 and sort descending by cosine similarity
    vec_scored.retain(|(_, sim)| *sim >= COS_FLOOR);
    vec_scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut scores: HashMap<K, f64> = HashMap::new();

    for (idx, key) in fts_ranked.iter().enumerate() {
        let rank = (idx + 1) as f64;
        *scores.entry(key.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank);
    }

    for (idx, (key, _sim)) in vec_scored.iter().enumerate() {
        let rank = (idx + 1) as f64;
        *scores.entry(key.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank);
    }

    let mut combined: Vec<(K, f64)> = scores.into_iter().collect();
    combined.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    combined
}

/// Truncates or selects formatted blocks so the total output stays within `budget_tokens` (1 token ≈ 4 bytes).
pub fn apply_token_budget(blocks: &[String], budget_tokens: usize) -> String {
    let max_bytes = budget_tokens.saturating_mul(4).max(240);
    let mut out = String::new();

    for block in blocks {
        if out.len() + block.len() > max_bytes {
            if out.is_empty() {
                let take_len = max_bytes.min(block.len());
                let safe_slice = truncate_utf8(block, take_len);
                out.push_str(safe_slice);
                out.push_str("\n... [truncated to token budget]\n");
            }
            break;
        }
        out.push_str(block);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }

    out
}

fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_and_rrf_with_cos_floor() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let c = vec![0.4, 0.9165, 0.0]; // cos = 0.4 < 0.53

        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-5);
        assert!(cosine_similarity(&a, &c) < COS_FLOOR);

        let fts = vec![10i64, 20i64];
        let vec_res = vec![(20i64, 0.88), (30i64, 0.51), (40i64, 0.65)];
        let fused = reciprocal_rank_fusion(&fts, vec_res);

        // Item 20 is in both FTS (rank 2) and Vec (rank 1), so it must rank #1
        assert_eq!(fused[0].0, 20);
        // Item 30 has sim 0.51 < 0.53, so it must be excluded
        assert!(!fused.iter().any(|(id, _)| *id == 30));
    }
}
