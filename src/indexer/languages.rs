use crate::indexer::markdown::{ExtractedSymbol, ExtractedSymbolRef};
use anyhow::{bail, Result};
use std::collections::HashSet;
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
    let (syms, _) = extract_code_symbols_and_refs(lang, source)?;
    Ok(syms)
}

pub fn extract_code_symbols_and_refs(
    lang: CodeLanguage,
    source: &str,
) -> Result<(Vec<ExtractedSymbol>, Vec<ExtractedSymbolRef>)> {
    if lang == CodeLanguage::Svelte {
        return extract_svelte_symbols_and_refs(source);
    }
    parse_with_offset(lang, source, 0)
}

fn parse_with_offset(
    lang: CodeLanguage,
    source: &str,
    line_offset: usize,
) -> Result<(Vec<ExtractedSymbol>, Vec<ExtractedSymbolRef>)> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang.ts_language())
        .map_err(|e| anyhow::anyhow!("Failed to load tree-sitter grammar: {e}"))?;

    let Some(tree) = parser.parse(source, None) else {
        bail!("Tree-sitter failed to parse source");
    };

    let lines: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();
    let mut refs_out = Vec::new();
    walk_node(
        lang,
        tree.root_node(),
        source.as_bytes(),
        &lines,
        line_offset,
        None,
        None,
        &mut out,
        &mut refs_out,
    );
    dedup_refs(&mut refs_out);
    Ok((out, refs_out))
}

fn extract_svelte_symbols_and_refs(
    source: &str,
) -> Result<(Vec<ExtractedSymbol>, Vec<ExtractedSymbolRef>)> {
    let mut symbols = Vec::new();
    let mut refs = Vec::new();
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
                    if let Ok((mut syms, mut rfs)) =
                        parse_with_offset(CodeLanguage::TypeScript, &block_src, script_start_line)
                    {
                        symbols.append(&mut syms);
                        refs.append(&mut rfs);
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
                    if let Ok((mut syms, mut rfs)) =
                        parse_with_offset(CodeLanguage::TypeScript, &block_src, script_start_line)
                    {
                        symbols.append(&mut syms);
                        refs.append(&mut rfs);
                    }
                }
            }
        }
        idx += 1;
    }

    dedup_refs(&mut refs);
    Ok((symbols, refs))
}

fn dedup_refs(refs: &mut Vec<ExtractedSymbolRef>) {
    let mut seen = HashSet::with_capacity(refs.len());
    refs.retain(|r| seen.insert((r.symbol_name.clone(), r.line_number)));
}

#[allow(clippy::too_many_arguments)]
fn walk_node(
    lang: CodeLanguage,
    node: Node,
    source_bytes: &[u8],
    lines: &[&str],
    line_offset: usize,
    parent_name: Option<&str>,
    caller_fn: Option<&str>,
    out: &mut Vec<ExtractedSymbol>,
    refs_out: &mut Vec<ExtractedSymbolRef>,
) {
    let kind = node.kind();
    let mut next_parent: Option<String> = parent_name.map(|s| s.to_string());
    let mut next_caller: Option<String> = caller_fn.map(|s| s.to_string());
    let line_number = node.start_position().row + 1 + line_offset;

    match lang {
        CodeLanguage::Rust => {
            if kind == "use_declaration" {
                extract_rust_use_refs(node, source_bytes, line_offset, caller_fn, refs_out);
                return;
            }

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
                        next_parent = Some(name.clone());
                    }
                    if sym_kind == "fn" {
                        next_caller = Some(name);
                    }
                }
            }

            match kind {
                "call_expression" => {
                    if let Some(raw_fn) = node.child_by_field_name("function") {
                        let fn_node = if raw_fn.kind() == "generic_function" {
                            raw_fn.child_by_field_name("function").unwrap_or(raw_fn)
                        } else {
                            raw_fn
                        };
                        match fn_node.kind() {
                            "identifier" => {
                                if let Ok(txt) = fn_node.utf8_text(source_bytes) {
                                    push_ref(refs_out, txt.trim(), caller_fn, line_number, "call");
                                }
                            }
                            "field_expression" => {
                                if let Some(f) = node_field_text(fn_node, "field", source_bytes) {
                                    push_ref(refs_out, &f, caller_fn, line_number, "call");
                                }
                            }
                            "scoped_identifier" => {
                                if let Some(path_node) = fn_node.child_by_field_name("path") {
                                    if let Some(type_or_mod) = terminal_identifier(path_node, source_bytes) {
                                        if is_upper_ident(&type_or_mod) {
                                            push_ref(refs_out, &type_or_mod, caller_fn, line_number, "call");
                                        }
                                    }
                                }
                                if let Some(nm) = node_field_text(fn_node, "name", source_bytes) {
                                    push_ref(refs_out, &nm, caller_fn, line_number, "call");
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "type_identifier" => {
                    if !is_type_declaration_name(node) {
                        if let Ok(txt) = node.utf8_text(source_bytes) {
                            push_ref(refs_out, txt.trim(), caller_fn, line_number, "type_use");
                        }
                    }
                }
                "scoped_identifier" => {
                    let parent_kind = node.parent().map(|p| p.kind()).unwrap_or("");
                    if !matches!(parent_kind, "call_expression" | "generic_function" | "scoped_identifier") {
                        if let Some(path_node) = node.child_by_field_name("path") {
                            if let Some(type_name) = terminal_identifier(path_node, source_bytes) {
                                if is_upper_ident(&type_name) {
                                    push_ref(refs_out, &type_name, caller_fn, line_number, "type_use");
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        CodeLanguage::TypeScript | CodeLanguage::Tsx | CodeLanguage::JavaScript | CodeLanguage::Svelte => {
            if kind == "import_statement" {
                extract_ts_import_refs(node, source_bytes, line_offset, caller_fn, refs_out);
                return;
            }
            match kind {
                "function_declaration" | "method_definition" => {
                    let name = node_field_text(node, "name", source_bytes)
                        .unwrap_or_else(|| first_identifier_child(node, source_bytes));
                    if !name.is_empty() {
                        out.push(build_symbol(node, lines, line_offset, name.clone(), "fn", parent_name));
                        next_caller = Some(name);
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
                                        name.clone(),
                                        "fn",
                                        parent_name,
                                    ));
                                    next_caller = Some(name);
                                }
                            }
                        }
                    }
                }
                "call_expression" | "new_expression" => {
                    let field = if kind == "new_expression" { "constructor" } else { "function" };
                    if let Some(fn_node) = node.child_by_field_name(field) {
                        match fn_node.kind() {
                            "identifier" => {
                                if let Ok(txt) = fn_node.utf8_text(source_bytes) {
                                    push_ref(refs_out, txt.trim(), caller_fn, line_number, "call");
                                }
                            }
                            "member_expression" => {
                                if let Some(prop) = node_field_text(fn_node, "property", source_bytes) {
                                    push_ref(refs_out, &prop, caller_fn, line_number, "call");
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "type_identifier" => {
                    if !is_type_declaration_name(node) {
                        if let Ok(txt) = node.utf8_text(source_bytes) {
                            push_ref(refs_out, txt.trim(), caller_fn, line_number, "type_use");
                        }
                    }
                }
                _ => {}
            }
        }
        CodeLanguage::Python => match kind {
            "import_from_statement" => {
                let mod_id = node.child_by_field_name("module_name").map(|n| n.id());
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if Some(child.id()) != mod_id && (child.kind() == "dotted_name" || child.kind() == "aliased_import") {
                        if let Some(ident) = terminal_identifier(child, source_bytes) {
                            push_ref(refs_out, &ident, caller_fn, line_number, "import");
                        }
                    }
                }
                return;
            }
            "function_definition" => {
                let name = node_field_text(node, "name", source_bytes)
                    .unwrap_or_else(|| first_identifier_child(node, source_bytes));
                if !name.is_empty() {
                    out.push(build_symbol(node, lines, line_offset, name.clone(), "fn", parent_name));
                    next_caller = Some(name);
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
            "call" => {
                if let Some(fn_node) = node.child_by_field_name("function") {
                    match fn_node.kind() {
                        "identifier" => {
                            if let Ok(txt) = fn_node.utf8_text(source_bytes) {
                                push_ref(refs_out, txt.trim(), caller_fn, line_number, "call");
                            }
                        }
                        "attribute" => {
                            if let Some(attr) = node_field_text(fn_node, "attribute", source_bytes) {
                                push_ref(refs_out, &attr, caller_fn, line_number, "call");
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        },
        CodeLanguage::Go => match kind {
            "function_declaration" | "method_declaration" => {
                let name = node_field_text(node, "name", source_bytes)
                    .unwrap_or_else(|| first_identifier_child(node, source_bytes));
                if !name.is_empty() {
                    out.push(build_symbol(node, lines, line_offset, name.clone(), "fn", parent_name));
                    next_caller = Some(name);
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
            "call_expression" => {
                if let Some(fn_node) = node.child_by_field_name("function") {
                    match fn_node.kind() {
                        "identifier" => {
                            if let Ok(txt) = fn_node.utf8_text(source_bytes) {
                                push_ref(refs_out, txt.trim(), caller_fn, line_number, "call");
                            }
                        }
                        "selector_expression" => {
                            if let Some(fld) = node_field_text(fn_node, "field", source_bytes) {
                                push_ref(refs_out, &fld, caller_fn, line_number, "call");
                            }
                        }
                        _ => {}
                    }
                }
            }
            "type_identifier" => {
                if !is_type_declaration_name(node) {
                    if let Ok(txt) = node.utf8_text(source_bytes) {
                        push_ref(refs_out, txt.trim(), caller_fn, line_number, "type_use");
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
            next_caller.as_deref(),
            out,
            refs_out,
        );
    }
}

fn push_ref(
    refs_out: &mut Vec<ExtractedSymbolRef>,
    symbol_name: &str,
    caller_fn: Option<&str>,
    line_number: usize,
    ref_kind: &str,
) {
    if symbol_name.is_empty()
        || matches!(symbol_name, "self" | "Self" | "crate" | "super" | "_")
    {
        return;
    }
    refs_out.push(ExtractedSymbolRef {
        symbol_name: symbol_name.to_string(),
        caller_symbol: caller_fn.map(|s| s.to_string()),
        line_number,
        ref_kind: ref_kind.to_string(),
    });
}

fn is_upper_ident(s: &str) -> bool {
    s.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false) && s != "Self"
}

fn is_type_declaration_name(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let pk = parent.kind();
    if matches!(
        pk,
        "type_parameters" | "type_parameter" | "constrained_type_parameter"
    ) {
        return true;
    }
    if matches!(
        pk,
        "struct_item"
            | "enum_item"
            | "trait_item"
            | "type_item"
            | "class_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration"
            | "type_spec"
            | "type_alias"
    ) {
        if let Some(name_node) = parent.child_by_field_name("name") {
            if name_node.id() == node.id() {
                return true;
            }
        }
    }
    false
}

fn terminal_identifier(node: Node, source_bytes: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" | "field_identifier" => node
            .utf8_text(source_bytes)
            .ok()
            .map(|s| s.trim().to_string()),
        "scoped_identifier" | "scoped_type_identifier" => node
            .child_by_field_name("name")
            .and_then(|n| terminal_identifier(n, source_bytes)),
        _ => {
            let mut last = None;
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if let Some(id) = terminal_identifier(child, source_bytes) {
                    last = Some(id);
                }
            }
            last
        }
    }
}

fn extract_rust_use_refs(
    node: Node,
    source_bytes: &[u8],
    line_offset: usize,
    caller_fn: Option<&str>,
    refs_out: &mut Vec<ExtractedSymbolRef>,
) {
    let line_number = node.start_position().row + 1 + line_offset;
    match node.kind() {
        "scoped_identifier" => {
            if let Some(name) = node_field_text(node, "name", source_bytes) {
                push_ref(refs_out, &name, caller_fn, line_number, "import");
            }
        }
        "scoped_use_list" => {
            if let Some(list) = node.child_by_field_name("list") {
                extract_rust_use_refs(list, source_bytes, line_offset, caller_fn, refs_out);
            }
        }
        "use_as_clause" => {
            if let Some(path) = node.child_by_field_name("path") {
                extract_rust_use_refs(path, source_bytes, line_offset, caller_fn, refs_out);
            }
        }
        "identifier" | "type_identifier" => {
            if let Ok(txt) = node.utf8_text(source_bytes) {
                push_ref(refs_out, txt.trim(), caller_fn, line_number, "import");
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_rust_use_refs(child, source_bytes, line_offset, caller_fn, refs_out);
            }
        }
    }
}

fn extract_ts_import_refs(
    node: Node,
    source_bytes: &[u8],
    line_offset: usize,
    caller_fn: Option<&str>,
    refs_out: &mut Vec<ExtractedSymbolRef>,
) {
    let line_number = node.start_position().row + 1 + line_offset;
    if node.kind() == "identifier" {
        if let Ok(txt) = node.utf8_text(source_bytes) {
            push_ref(refs_out, txt.trim(), caller_fn, line_number, "import");
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "string" {
            extract_ts_import_refs(child, source_bytes, line_offset, caller_fn, refs_out);
        }
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
