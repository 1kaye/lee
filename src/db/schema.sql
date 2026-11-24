-- 1. File Hash Tracker for Lazy Re-indexing (Source Code & .md/.mdx Documentation)
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

-- 3. Documentation Section Metadata (.md / .mdx Sections & Registered Doc Code Files)
CREATE TABLE IF NOT EXISTS doc_sections_meta (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_path TEXT NOT NULL,
    heading TEXT NOT NULL,       -- Section heading or code filename
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    content TEXT NOT NULL,       -- Section body text or full code content
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

-- 6. Cross-File Symbol Reference & Call Graph Index
CREATE TABLE IF NOT EXISTS symbol_refs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    symbol_name TEXT NOT NULL,
    file_path TEXT NOT NULL,
    caller_symbol TEXT,
    line_number INTEGER NOT NULL,
    ref_kind TEXT NOT NULL,      -- 'call', 'type_use', 'import'
    FOREIGN KEY (file_path) REFERENCES file_hashes(file_path) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_symbol_refs_name ON symbol_refs(symbol_name);
CREATE INDEX IF NOT EXISTS idx_symbol_refs_file ON symbol_refs(file_path);
