-- 1. File Hash Tracker for Lazy Re-indexing (Source Code & .md Documentation)
CREATE TABLE IF NOT EXISTS file_hashes (
    file_path TEXT PRIMARY KEY,
    blake3_hash TEXT NOT NULL,
    line_count INTEGER NOT NULL,
    file_type TEXT NOT NULL,     -- 'code', 'markdown', or 'config'
    last_indexed INTEGER NOT NULL
);

-- 2. Cross-Language AST Symbol Index & Markdown Headings
CREATE TABLE IF NOT EXISTS symbols (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_path TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,          -- 'fn', 'struct', 'enum', 'trait', 'impl', 'interface', 'class', 'type', 'mod', 'md_heading'
    signature TEXT NOT NULL,     -- Declaration line for code symbols or full heading text (e.g. '## Grid Layout')
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    parent_name TEXT,            -- Enclosing impl/class or parent heading if any
    FOREIGN KEY (file_path) REFERENCES file_hashes(file_path) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_path);

-- 3. Documentation Section Metadata & Vector Embeddings (.md Sections)
CREATE TABLE IF NOT EXISTS doc_sections_meta (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_path TEXT NOT NULL,
    heading TEXT NOT NULL,       -- Section heading (e.g. '### Pixel Spacing & Margin')
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    content TEXT NOT NULL,       -- Section body text and code snippets
    embedding BLOB,              -- BGE-Small [f32; 384] array in little-endian bytes (1,536 bytes)
    FOREIGN KEY (file_path) REFERENCES file_hashes(file_path) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_doc_sections_file ON doc_sections_meta(file_path);

-- 4. Full-Text Search Index for Documentation (FTS5 BM25)
CREATE VIRTUAL TABLE IF NOT EXISTS doc_sections_fts USING fts5(
    file_path,
    heading,
    content,
    content='doc_sections_meta',
    content_rowid='id',
    tokenize='porter unicode61'
);

-- Auto-sync triggers for doc_sections_meta -> doc_sections_fts
CREATE TRIGGER IF NOT EXISTS doc_sections_ai AFTER INSERT ON doc_sections_meta BEGIN
    INSERT INTO doc_sections_fts(rowid, file_path, heading, content)
    VALUES (new.id, new.file_path, new.heading, new.content);
END;

CREATE TRIGGER IF NOT EXISTS doc_sections_ad AFTER DELETE ON doc_sections_meta BEGIN
    INSERT INTO doc_sections_fts(doc_sections_fts, rowid, file_path, heading, content)
    VALUES ('delete', old.id, old.file_path, old.heading, old.content);
END;

-- 5. Large Output Handle Store (Handles h:ID)
CREATE TABLE IF NOT EXISTS handles (
    id TEXT PRIMARY KEY,         -- Format: 'h:' + 6 hex chars of blake3 (e.g. 'h:9f3a1b')
    source_cmd TEXT NOT NULL,
    exit_code INTEGER,
    total_lines INTEGER NOT NULL,
    raw_content TEXT NOT NULL,
    digest_text TEXT NOT NULL,
    failures_text TEXT,
    created_at INTEGER NOT NULL
);

-- 6. Memory Metadata & BGE-Small Vector Embeddings (384-dim)
CREATE TABLE IF NOT EXISTS memories_meta (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope TEXT NOT NULL,         -- 'project', 'global', 'handoff', or 'backlog'
    topic TEXT NOT NULL,
    tags TEXT NOT NULL,
    content TEXT NOT NULL,
    md_path TEXT,                -- Path to backing Markdown file on disk
    embedding BLOB,              -- [f32; 384] array in little-endian bytes (1,536 bytes)
    created_at INTEGER NOT NULL
);

-- 7. Full-Text Search Index for Memories (FTS5 BM25)
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    topic,
    tags,
    content,
    content='memories_meta',
    content_rowid='id',
    tokenize='porter unicode61'
);

-- Auto-sync triggers for memories_meta -> memories_fts
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories_meta BEGIN
    INSERT INTO memories_fts(rowid, topic, tags, content)
    VALUES (new.id, new.topic, new.tags, new.content);
END;

CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories_meta BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, topic, tags, content)
    VALUES ('delete', old.id, old.topic, old.tags, old.content);
END;

-- 8. Token Savings Statistics (Audit & Metrics)
CREATE TABLE IF NOT EXISTS token_metrics (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,
    tool_name TEXT NOT NULL,
    raw_bytes INTEGER NOT NULL,  -- Unfiltered payload size (full file / codebase / raw terminal log)
    sent_bytes INTEGER NOT NULL, -- Compact summary/slice size actually sent to the AI
    saved_bytes INTEGER NOT NULL -- Saved byte difference (1 token ~= 4 bytes)
);
