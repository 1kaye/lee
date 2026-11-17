use crate::indexer::languages::{extract_code_symbols, CodeLanguage};
use crate::indexer::markdown::{parse_markdown, DocSectionChunk, ExtractedSymbol};
use anyhow::Result;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Toml,
    Yaml,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexedFileType {
    Code(CodeLanguage),
    Markdown,
    Config(ConfigFormat),
}

impl IndexedFileType {
    pub fn classify(path: &Path) -> Option<Self> {
        if let Some(lang) = CodeLanguage::from_path(path) {
            return Some(Self::Code(lang));
        }
        let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if matches!(fname, "package-lock.json" | "Cargo.lock" | "pnpm-lock.yaml" | "yarn.lock") {
            return None;
        }
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        if matches!(ext.as_str(), "md" | "mdx" | "markdown") {
            return Some(Self::Markdown);
        }
        match ext.as_str() {
            "toml" => Some(Self::Config(ConfigFormat::Toml)),
            "yml" | "yaml" => Some(Self::Config(ConfigFormat::Yaml)),
            "json" => Some(Self::Config(ConfigFormat::Json)),
            _ => None,
        }
    }

    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Code(_) => "code",
            Self::Markdown => "markdown",
            Self::Config(_) => "config",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedFileResult {
    pub file_type: IndexedFileType,
    pub line_count: usize,
    pub symbols: Vec<ExtractedSymbol>,
    pub doc_sections: Vec<DocSectionChunk>,
}

pub fn parse_file_content(path: &Path, source: &str) -> Result<Option<ParsedFileResult>> {
    let Some(ft) = IndexedFileType::classify(path) else {
        return Ok(None);
    };
    let line_count = source.lines().count().max(1);

    match ft {
        IndexedFileType::Code(lang) => {
            let symbols = extract_code_symbols(lang, source)?;
            Ok(Some(ParsedFileResult {
                file_type: ft,
                line_count,
                symbols,
                doc_sections: Vec::new(),
            }))
        }
        IndexedFileType::Markdown => {
            let parsed = parse_markdown(source);
            Ok(Some(ParsedFileResult {
                file_type: ft,
                line_count,
                symbols: parsed.symbols,
                doc_sections: parsed.sections,
            }))
        }
        IndexedFileType::Config(fmt) => {
            let symbols = extract_config_symbols(fmt, source);
            Ok(Some(ParsedFileResult {
                file_type: ft,
                line_count,
                symbols,
                doc_sections: Vec::new(),
            }))
        }
    }
}

fn extract_config_symbols(fmt: ConfigFormat, source: &str) -> Vec<ExtractedSymbol> {
    let lines: Vec<&str> = source.lines().collect();
    let total_lines = lines.len().max(1);
    let mut raw: Vec<(String, String, usize, Option<String>)> = Vec::new();

    match fmt {
        ConfigFormat::Toml => {
            for (idx, line) in lines.iter().enumerate() {
                let t = line.trim();
                if t.starts_with('[') && t.ends_with(']') && !t.starts_with('#') {
                    let name = t.trim_matches(|c| c == '[' || c == ']').trim().to_string();
                    if !name.is_empty() {
                        raw.push((name, t.to_string(), idx + 1, None));
                    }
                }
            }
        }
        ConfigFormat::Yaml => {
            let mut top_parent: Option<String> = None;
            for (idx, line) in lines.iter().enumerate() {
                if line.trim().is_empty() || line.trim_start().starts_with('#') {
                    continue;
                }
                let indent = line.len() - line.trim_start().len();
                let trimmed = line.trim();
                if indent <= 2 && !trimmed.starts_with('-') {
                    if let Some((key, _)) = trimmed.split_once(':') {
                        let clean_key = key.trim().trim_matches(|c| c == '"' || c == '\'');
                        if !clean_key.is_empty() {
                            if indent == 0 {
                                top_parent = Some(clean_key.to_string());
                                raw.push((clean_key.to_string(), trimmed.to_string(), idx + 1, None));
                            } else {
                                raw.push((
                                    clean_key.to_string(),
                                    trimmed.to_string(),
                                    idx + 1,
                                    top_parent.clone(),
                                ));
                            }
                        }
                    }
                }
            }
        }
        ConfigFormat::Json => {
            for (idx, line) in lines.iter().enumerate() {
                let indent = line.len() - line.trim_start().len();
                let trimmed = line.trim();
                // Capture top-level or 1st-nested JSON keys with compact schema summary
                if indent <= 4 && trimmed.starts_with('"') {
                    if let Some((key_part, val_part)) = trimmed.split_once(':') {
                        let key = key_part.trim().trim_matches('"');
                        if !key.is_empty() {
                            let val_trim = val_part.trim().trim_end_matches(',');
                            let val_hint = if val_trim.starts_with('{') {
                                "{...}"
                            } else if val_trim.starts_with('[') {
                                "[...]"
                            } else {
                                val_trim
                            };
                            raw.push((
                                key.to_string(),
                                format!("{key}: {val_hint}"),
                                idx + 1,
                                None,
                            ));
                        }
                    }
                }
            }
        }
    }

    let mut out = Vec::with_capacity(raw.len());
    for i in 0..raw.len() {
        let (name, sig, st, parent) = &raw[i];
        let en = if i + 1 < raw.len() {
            raw[i + 1].2.saturating_sub(1).max(*st)
        } else {
            total_lines
        };
        out.push(ExtractedSymbol {
            name: name.clone(),
            kind: "key".to_string(),
            signature: if sig.chars().count() > 100 {
                format!("{}...", sig.chars().take(100).collect::<String>())
            } else {
                sig.clone()
            },
            start_line: *st,
            end_line: en,
            parent_name: parent.clone(),
        });
    }
    out
}

/// Checks if a line is a pure non-doc comment (preserving `///` and `//!` doc comments).
pub fn is_non_doc_comment_line(line: &str, hash_comments: bool) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    if hash_comments {
        return t.starts_with('#') && !t.starts_with("#!");
    }
    (t.starts_with("//") && !t.starts_with("///") && !t.starts_with("//!"))
        || (t.starts_with("/*") && t.ends_with("*/") && !t.starts_with("/**"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_ts_svelte_python_go_parsing() {
        // Rust
        let rs_code = r#"
pub struct Config {
    pub port: u16,
}

impl Config {
    pub fn new(port: u16) -> Self {
        Self { port }
    }
}
"#;
        let rs_res = parse_file_content(Path::new("src/lib.rs"), rs_code)
            .unwrap()
            .unwrap();
        assert!(rs_res.symbols.iter().any(|s| s.name == "Config" && s.kind == "struct"));
        assert!(rs_res.symbols.iter().any(|s| s.name == "new" && s.kind == "fn" && s.parent_name.as_deref() == Some("Config")));

        // TypeScript arrow function & interface
        let ts_code = r#"
export interface User {
    id: string;
}
export const fetchUser = async (id: string) => {
    return { id };
};
"#;
        let ts_res = parse_file_content(Path::new("src/user.ts"), ts_code)
            .unwrap()
            .unwrap();
        assert!(ts_res.symbols.iter().any(|s| s.name == "User" && s.kind == "interface"));
        assert!(ts_res.symbols.iter().any(|s| s.name == "fetchUser" && s.kind == "fn"));

        // Svelte with <script> line offset
        let svelte_code = r#"<div>
  <h1>Hello</h1>
</div>
<script lang="ts">
  function handleClick() {
    console.log("clicked");
  }
</script>
"#;
        let sv_res = parse_file_content(Path::new("src/App.svelte"), svelte_code)
            .unwrap()
            .unwrap();
        assert_eq!(sv_res.symbols.len(), 1);
        assert_eq!(sv_res.symbols[0].name, "handleClick");
        assert_eq!(sv_res.symbols[0].start_line, 5);
        assert_eq!(sv_res.symbols[0].end_line, 7);

        // Python
        let py_code = "class Worker:\n    def run(self):\n        pass\n";
        let py_res = parse_file_content(Path::new("app.py"), py_code)
            .unwrap()
            .unwrap();
        assert!(py_res.symbols.iter().any(|s| s.name == "Worker" && s.kind == "class"));
        assert!(py_res.symbols.iter().any(|s| s.name == "run" && s.kind == "fn"));

        // Go
        let go_code = "package main\ntype Server struct {}\nfunc (s *Server) Start() {}\n";
        let go_res = parse_file_content(Path::new("main.go"), go_code)
            .unwrap()
            .unwrap();
        assert!(go_res.symbols.iter().any(|s| s.name == "Server" && s.kind == "struct"));
        assert!(go_res.symbols.iter().any(|s| s.name == "Start" && s.kind == "fn"));

        // YAML & TOML config outline
        let yml_code = "ui:\n  button: \"Save\"\ncanvas:\n  zoom: 1.0\n";
        let yml_res = parse_file_content(Path::new("locales/ui.yml"), yml_code)
            .unwrap()
            .unwrap();
        assert!(yml_res.symbols.iter().any(|s| s.name == "ui" && s.start_line == 1));
        assert!(yml_res.symbols.iter().any(|s| s.name == "canvas" && s.start_line == 3));

        let toml_code = "[package]\nname = \"app\"\n[dependencies]\nserde = \"1.0\"\n";
        let toml_res = parse_file_content(Path::new("Cargo.toml"), toml_code)
            .unwrap()
            .unwrap();
        assert!(toml_res.symbols.iter().any(|s| s.name == "package" && s.end_line == 2));
        assert!(toml_res.symbols.iter().any(|s| s.name == "dependencies" && s.start_line == 3));
    }
}
