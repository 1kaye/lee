use anyhow::{Context, Result};
use fastembed::{
    EmbeddingModel, ExecutionProviderDispatch, InitOptionsUserDefined, Pooling, QuantizationMode,
    TextEmbedding, TextInitOptions, TokenizerFiles, UserDefinedEmbeddingModel,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const EMBEDDING_DIM: usize = 384;
pub const EMBEDDING_BLOB_BYTES: usize = EMBEDDING_DIM * 4; // 1,536 bytes
pub const MAX_PASSAGE_CHARS: usize = 240;
pub const MAX_TOKEN_LENGTH: usize = 64;

static GLOBAL_EMBEDDER: OnceLock<Option<Mutex<TextEmbedding>>> = OnceLock::new();

/// Returns CUDA + CPU execution providers: ONNX Runtime tries CUDA first (if GPU + CUDA/cuDNN libs exist)
/// and automatically falls back to CPUExecutionProvider if CUDA runtime libraries are unavailable.
fn preferred_execution_providers() -> Vec<ExecutionProviderDispatch> {
    vec![
        ort::ep::CUDA::default().build(),
        ort::ep::CPU::default().build(),
    ]
}

/// Finds the local snapshot directory containing `model_optimized.onnx` and `tokenizer.json`
/// inside `~/.config/lee/models` without making any network calls.
pub fn find_cached_snapshot_dir(models_dir: &Path) -> Option<PathBuf> {
    if !models_dir.is_dir() {
        return None;
    }
    find_snapshot_recursive(models_dir, 0)
}

fn find_snapshot_recursive(dir: &Path, depth: usize) -> Option<PathBuf> {
    if depth > 5 {
        return None;
    }
    if dir.join("model_optimized.onnx").exists() && dir.join("tokenizer.json").exists() {
        return Some(dir.to_path_buf());
    }
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_snapshot_recursive(&path, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

/// Checks if the model cache directory already contains downloaded ONNX model files.
pub fn is_model_cached(models_dir: &Path) -> bool {
    find_cached_snapshot_dir(models_dir).is_some()
}

/// Loads `BGESmallENV15Q` 100% offline directly from disk (`try_new_from_user_defined`),
/// completely bypassing `hf-hub` so zero HTTP/DNS calls ever occur at query/index time.
fn load_offline_from_snapshot(snapshot_dir: &Path) -> Result<TextEmbedding> {
    let onnx_file = fs::read(snapshot_dir.join("model_optimized.onnx"))
        .context("Failed to read cached model_optimized.onnx")?;
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: fs::read(snapshot_dir.join("tokenizer.json"))
            .context("Failed to read cached tokenizer.json")?,
        config_file: fs::read(snapshot_dir.join("config.json"))
            .context("Failed to read cached config.json")?,
        special_tokens_map_file: fs::read(snapshot_dir.join("special_tokens_map.json"))
            .context("Failed to read cached special_tokens_map.json")?,
        tokenizer_config_file: fs::read(snapshot_dir.join("tokenizer_config.json"))
            .context("Failed to read cached tokenizer_config.json")?,
    };

    let user_model = UserDefinedEmbeddingModel::new(onnx_file, tokenizer_files)
        .with_pooling(Pooling::Cls)
        .with_quantization(QuantizationMode::Static);

    let cuda_opts = InitOptionsUserDefined::new()
        .with_max_length(MAX_TOKEN_LENGTH)
        .with_intra_threads(4)
        .with_execution_providers(preferred_execution_providers());

    if let Ok(model) = TextEmbedding::try_new_from_user_defined(user_model.clone(), cuda_opts) {
        return Ok(model);
    }

    let cpu_opts = InitOptionsUserDefined::new()
        .with_max_length(MAX_TOKEN_LENGTH)
        .with_intra_threads(4);

    TextEmbedding::try_new_from_user_defined(user_model, cpu_opts)
        .context("Failed to initialize offline BGESmallENV15Q session")
}

/// Explicitly downloads (if not yet cached) and verifies `EmbeddingModel::BGESmallENV15Q`
/// during `lee init`.
pub fn download_and_verify_model(models_dir: &Path, show_progress: bool) -> Result<()> {
    fs::create_dir_all(models_dir)
        .with_context(|| format!("Failed to create models dir {}", models_dir.display()))?;

    let mut model = if let Some(snap) = find_cached_snapshot_dir(models_dir) {
        load_offline_from_snapshot(&snap)?
    } else {
        let opts = TextInitOptions::new(EmbeddingModel::BGESmallENV15Q)
            .with_cache_dir(models_dir.to_path_buf())
            .with_max_length(MAX_TOKEN_LENGTH)
            .with_intra_threads(4)
            .with_execution_providers(preferred_execution_providers())
            .with_show_download_progress(show_progress);

        TextEmbedding::try_new(opts)
            .context("Failed to download/initialize BAAI/bge-small-en-v1.5 (BGESmallENV15Q)")?
    };

    let test_vecs = model
        .embed(vec!["query: health check"], None)
        .context("Failed to run verification embedding")?;

    if test_vecs.first().map(|v| v.len()) != Some(EMBEDDING_DIM) {
        anyhow::bail!("Unexpected embedding dimension from BGESmallENV15Q");
    }

    let _ = GLOBAL_EMBEDDER.set(Some(Mutex::new(model)));
    Ok(())
}

fn get_or_load_offline_embedder(models_dir: &Path) -> Option<&'static Mutex<TextEmbedding>> {
    let opt = GLOBAL_EMBEDDER.get_or_init(|| {
        let snap = find_cached_snapshot_dir(models_dir)?;
        load_offline_from_snapshot(&snap).ok().map(Mutex::new)
    });
    opt.as_ref()
}

pub fn cap_passage_chars(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        return s;
    }
    let mut end = max_chars;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Embeds a batch of pre-formatted strings (expects length-sorted batches to eliminate padding overhead).
pub fn try_embed_batch(models_dir: &Path, formatted_texts: Vec<String>) -> Option<Vec<Vec<f32>>> {
    if formatted_texts.is_empty() {
        return Some(Vec::new());
    }
    let mutex = get_or_load_offline_embedder(models_dir)?;
    let mut guard = mutex.lock().ok()?;
    let vecs = guard.embed(formatted_texts, Some(32)).ok()?;
    Some(vecs)
}

/// Embeds arbitrary pre-formatted text if the offline model is available; returns `None` for graceful FTS5 fallback.
pub fn try_embed_text(models_dir: &Path, formatted_text: &str) -> Option<Vec<f32>> {
    let mutex = get_or_load_offline_embedder(models_dir)?;
    let mut guard = mutex.lock().ok()?;
    let mut vecs = guard.embed(vec![formatted_text], None).ok()?;
    let vec = vecs.pop()?;
    if vec.len() == EMBEDDING_DIM {
        Some(vec)
    } else {
        None
    }
}

/// Formats and embeds a memory note using standard BGE prefix:
/// `"passage: {topic} [{tags}] - {content}"`
pub fn embed_memory_passage(
    models_dir: &Path,
    topic: &str,
    tags: &str,
    content: &str,
) -> Option<Vec<f32>> {
    let snippet = cap_passage_chars(content, MAX_PASSAGE_CHARS);
    let text = format!("passage: {topic} [{tags}] - {snippet}");
    try_embed_text(models_dir, &text)
}

/// Formats and embeds a Markdown section chunk using standard BGE prefix:
/// `"passage: {file_path} > {heading} - {content}"`
pub fn embed_doc_section_passage(
    models_dir: &Path,
    file_path: &str,
    heading: &str,
    content: &str,
) -> Option<Vec<f32>> {
    let snippet = cap_passage_chars(content, MAX_PASSAGE_CHARS);
    let text = format!("passage: {file_path} > {heading} - {snippet}");
    try_embed_text(models_dir, &text)
}

/// Formats and embeds a search query using standard BGE prefix:
/// `"query: {query}"`
pub fn embed_query(models_dir: &Path, query: &str) -> Option<Vec<f32>> {
    let text = format!("query: {query}");
    try_embed_text(models_dir, &text)
}

/// Serializes `&[f32]` (384 dims) into little-endian bytes (`Vec<u8>`, 1,536 bytes).
pub fn serialize_f32_vec(vec: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vec.len() * 4);
    for &val in vec {
        bytes.extend_from_slice(&val.to_le_bytes());
    }
    bytes
}

/// Deserializes SQLite `BLOB` (`&[u8]`) back into `Vec<f32>` using `as_chunks::<4>()` and `f32::from_le_bytes`.
pub fn deserialize_f32_vec(blob: &[u8]) -> Vec<f32> {
    let (chunks, _) = blob.as_chunks::<4>();
    chunks.iter().copied().map(f32::from_le_bytes).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vector_blob_roundtrip_384_dim() {
        let original: Vec<f32> = (0..EMBEDDING_DIM).map(|i| (i as f32) * 0.0025 - 0.5).collect();
        let bytes = serialize_f32_vec(&original);
        assert_eq!(bytes.len(), EMBEDDING_BLOB_BYTES);

        let decoded = deserialize_f32_vec(&bytes);
        assert_eq!(decoded.len(), EMBEDDING_DIM);
        for (a, b) in original.iter().zip(decoded.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }
}
