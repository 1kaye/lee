use crate::indexer::markdown::ExtractedSymbol;
use anyhow::{bail, Result};
use std::path::Path;
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeLanguage {
    Rust,
    TypeScript,
    Tsx,
    JavaScript,
    Svelte,
    Python,
    Go,
}

impl CodeLanguage {
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Some(Self::Rust),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "svelte" => Some(Self::Svelte),
            "py" => Some(Self::Python),
            "go" => Some(Self::Go),
            _ => None,
        }
    }

    pub fn ts_language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::TypeScript | Self::Svelte => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }
}

pub fn extract_code_symbols(lang: CodeLanguage, source: &str) -> Result<Vec<ExtractedSymbol>> {
    if lang == CodeLanguage::Svelte {
        return extract_svelte_symbols(source);
    }
    parse_with_offset(lang, source, 0)
}

fn parse_with_offset(
    lang: CodeLanguage,
    source: &str,
    line_offset: usize,
) -> Result<Vec<ExtractedSymbol>> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang.ts_language())
        .map_err(|e| anyhow::anyhow!("Failed to load tree-sitter grammar: {e}"))?;

    let Some(tree) = parser.parse(source, None) else {
        bail!("Tree-sitter failed to parse source");
    };

    let lines: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();
    walk_node(lang, tree.root_node(), source.as_bytes(), &lines, line_offset, None, &mut out);
    Ok(out)
}

fn extract_svelte_symbols(source: &str) -> Result<Vec<ExtractedSymbol>> {
    let mut symbols = Vec::new();
    let lines: Vec<&str> = source.lines().collect();
    let mut idx = 0;

    while idx < lines.len() {
        let line = lines[idx];
        if let Some(open_pos) = line.find("<script") {
            let after_open = &line[open_pos..];
            if let Some(tag_end_rel) = after_open.find('>') {
                let script_start_line = idx; // 0-based line offset for the <script> line
                let mut block_lines = Vec::new();
                let first_line_rest = &after_open[tag_end_rel + 1..];

                if let Some(close_rel) = first_line_rest.find("</script>") {
                    block_lines.push(&first_line_rest[..close_rel]);
                    let block_src = block_lines.join("\n");
                    if let Ok(mut syms) =
                        parse_with_offset(CodeLanguage::TypeScript, &block_src, script_start_line)
                    {
                        symbols.append(&mut syms);
                    }
                    idx += 1;
                    continue;
                } else {
                    block_lines.push(first_line_rest);
                    idx += 1;
                    while idx < lines.len() {
                        if let Some(close_pos) = lines[idx].find("</script>") {
                            block_lines.push(&lines[idx][..close_pos]);
                            break;
                        }
                        block_lines.push(lines[idx]);
                        idx += 1;
                    }
                    let block_src = block_lines.join("\n");
                    if let Ok(mut syms) =
                        parse_with_offset(CodeLanguage::TypeScript, &block_src, script_start_line)
                    {
                        symbols.append(&mut syms);
                    }
                }
            }
        }
        idx += 1;
    }

    Ok(symbols)
}

fn walk_node(
    lang: CodeLanguage,
    node: Node,
    source_bytes: &[u8],
    lines: &[&str],
    line_offset: usize,
    parent_name: Option<&str>,
    out: &mut Vec<ExtractedSymbol>,
) {
    let kind = node.kind();
    let mut next_parent: Option<String> = parent_name.map(|s| s.to_string());

    match lang {
        CodeLanguage::Rust => {
            let mapped_kind = match kind {
                "struct_item" => Some("struct"),
                "enum_item" => Some("enum"),
                "trait_item" => Some("trait"),
                "impl_item" => Some("impl"),
                "function_item" | "function_signature_item" => Some("fn"),
                "mod_item" => Some("mod"),
                "type_item" => Some("type"),
                "const_item" | "static_item" => Some("const"),
                "macro_definition" => Some("macro"),
                _ => None,
            };

            if let Some(sym_kind) = mapped_kind {
                let name = if kind == "impl_item" {
                    node_field_text(node, "type", source_bytes)
                        .unwrap_or_else(|| "impl".to_string())
                } else {
                    node_field_text(node, "name", source_bytes)
                        .unwrap_or_else(|| first_identifier_child(node, source_bytes))
                };

                if !name.is_empty() {
                    let sym = build_symbol(node, lines, line_offset, name.clone(), sym_kind, parent_name);
                    out.push(sym);
                    if matches!(sym_kind, "impl" | "trait" | "mod" | "struct") {
                        next_parent = Some(name);
                    }
                }
            }
        }
        CodeLanguage::TypeScript | CodeLanguage::Tsx | CodeLanguage::JavaScript | CodeLanguage::Svelte => {
            match kind {
                "function_declaration" | "method_definition" => {
                    let name = node_field_text(node, "name", source_bytes)
                        .unwrap_or_else(|| first_identifier_child(node, source_bytes));
                    if !name.is_empty() {
                        out.push(build_symbol(node, lines, line_offset, name, "fn", parent_name));
                    }
                }
                "class_declaration" => {
                    let name = node_field_text(node, "name", source_bytes)
                        .unwrap_or_else(|| first_identifier_child(node, source_bytes));
                    if !name.is_empty() {
                        out.push(build_symbol(node, lines, line_offset, name.clone(), "class", parent_name));
                        next_parent = Some(name);
                    }
                }
                "interface_declaration" => {
                    let name = node_field_text(node, "name", source_bytes)
                        .unwrap_or_else(|| first_identifier_child(node, source_bytes));
                    if !name.is_empty() {
                        out.push(build_symbol(
                            node,
                            lines,
                            line_offset,
                            name.clone(),
                            "interface",
                            parent_name,
                        ));
                        next_parent = Some(name);
                    }
                }
                "type_alias_declaration" => {
                    let name = node_field_text(node, "name", source_bytes)
                        .unwrap_or_else(|| first_identifier_child(node, source_bytes));
                    if !name.is_empty() {
                        out.push(build_symbol(node, lines, line_offset, name, "type", parent_name));
                    }
                }
                "enum_declaration" => {
                    let name = node_field_text(node, "name", source_bytes)
                        .unwrap_or_else(|| first_identifier_child(node, source_bytes));
                    if !name.is_empty() {
                        out.push(build_symbol(node, lines, line_offset, name, "enum", parent_name));
                    }
                }
                "lexical_declaration" | "variable_declaration" => {
                    // Check if any variable_declarator child contains an arrow_function
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        if child.kind() == "variable_declarator" {
                            let val_is_arrow = child
                                .child_by_field_name("value")
                                .map(|v| v.kind() == "arrow_function" || v.kind() == "function_expression")
                                .unwrap_or(false);
                            if val_is_arrow {
                                let name = node_field_text(child, "name", source_bytes)
                                    .unwrap_or_else(|| first_identifier_child(child, source_bytes));
                                if !name.is_empty() {
                                    out.push(build_symbol(
                                        node,
                                        lines,
                                        line_offset,
                                        name,
                                        "fn",
                                        parent_name,
                                    ));
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        CodeLanguage::Python => match kind {
            "function_definition" => {
                let name = node_field_text(node, "name", source_bytes)
                    .unwrap_or_else(|| first_identifier_child(node, source_bytes));
                if !name.is_empty() {
                    out.push(build_symbol(node, lines, line_offset, name, "fn", parent_name));
                }
            }
            "class_definition" => {
                let name = node_field_text(node, "name", source_bytes)
                    .unwrap_or_else(|| first_identifier_child(node, source_bytes));
                if !name.is_empty() {
                    out.push(build_symbol(node, lines, line_offset, name.clone(), "class", parent_name));
                    next_parent = Some(name);
                }
            }
            _ => {}
        },
        CodeLanguage::Go => match kind {
            "function_declaration" | "method_declaration" => {
                let name = node_field_text(node, "name", source_bytes)
                    .unwrap_or_else(|| first_identifier_child(node, source_bytes));
                if !name.is_empty() {
                    out.push(build_symbol(node, lines, line_offset, name, "fn", parent_name));
                }
            }
            "type_declaration" => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "type_spec" || child.kind() == "type_alias" {
                        let name = node_field_text(child, "name", source_bytes)
                            .unwrap_or_else(|| first_identifier_child(child, source_bytes));
                        let type_node_kind = child
                            .child_by_field_name("type")
                            .map(|t| t.kind())
                            .unwrap_or("");
                        let sym_kind = match type_node_kind {
                            "struct_type" => "struct",
                            "interface_type" => "interface",
                            _ => "type",
                        };
                        if !name.is_empty() {
                            out.push(build_symbol(
                                child,
                                lines,
                                line_offset,
                                name,
                                sym_kind,
                                parent_name,
                            ));
                        }
                    }
                }
            }
            _ => {}
        },
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_node(
            lang,
            child,
            source_bytes,
            lines,
            line_offset,
            next_parent.as_deref(),
            out,
        );
    }
}

fn node_field_text(node: Node, field: &str, source_bytes: &[u8]) -> Option<String> {
    let f = node.child_by_field_name(field)?;
    f.utf8_text(source_bytes).ok().map(|s| s.trim().to_string())
}

fn first_identifier_child(node: Node, source_bytes: &[u8]) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind().contains("identifier") {
            if let Ok(s) = child.utf8_text(source_bytes) {
                return s.trim().to_string();
            }
        }
    }
    String::new()
}

fn build_symbol(
    node: Node,
    lines: &[&str],
    line_offset: usize,
    name: String,
    kind: &str,
    parent_name: Option<&str>,
) -> ExtractedSymbol {
    let start_row = node.start_position().row;
    let end_row = node.end_position().row;
    let start_line = start_row + 1 + line_offset;
    let end_line = (end_row + 1 + line_offset).max(start_line);

    let first_line = lines.get(start_row).copied().unwrap_or("").trim();
    let signature = clean_signature(first_line);

    ExtractedSymbol {
        name,
        kind: kind.to_string(),
        signature,
        start_line,
        end_line,
        parent_name: parent_name.map(|s| s.to_string()),
    }
}

fn clean_signature(raw_line: &str) -> String {
    let trimmed = raw_line
        .trim_end_matches('{')
        .trim_end_matches(':')
        .trim();
    if trimmed.chars().count() > 140 {
        let short: String = trimmed.chars().take(140).collect();
        format!("{short}...")
    } else {
        trimmed.to_string()
    }
}
