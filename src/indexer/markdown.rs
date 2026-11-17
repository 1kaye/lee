#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedSymbol {
    pub name: String,
    pub kind: String,
    pub signature: String,
    pub start_line: usize,
    pub end_line: usize,
    pub parent_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocSectionChunk {
    pub heading: String,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ParsedMarkdown {
    pub symbols: Vec<ExtractedSymbol>,
    pub sections: Vec<DocSectionChunk>,
}

pub const SECTION_WINDOW_LINES: usize = 120;
pub const SECTION_OVERLAP_LINES: usize = 15;
pub const COALESCE_MAX_LINES: usize = 120;
pub const COALESCE_MAX_BYTES: usize = 3600;

struct RawHeading {
    level: usize,
    title: String,
    full_line: String,
    line_num: usize, // 1-based
    parent_name: Option<String>,
}

/// Parses a Markdown (`.md` / `.mdx`) document using a code-fence-aware state machine.
/// Tracks ` ``` ` and `~~~` fences so comments inside code blocks (e.g. `# bash comment`)
/// are never misidentified as Markdown headings.
/// - Every `#`..`####` heading is extracted into `symbols` for exact `outline` / `read_symbol`.
/// - Adjacent tiny sections are coalesced up to 45 lines / 1.4 KB to avoid thousands of tiny ONNX calls,
///   while very long sections (> 80 lines) are split into overlapping windows (80 lines, overlap 10).
pub fn parse_markdown(source: &str) -> ParsedMarkdown {
    let lines: Vec<&str> = source.lines().collect();
    let total_lines = lines.len().max(1);

    let mut in_fence = false;
    let mut fence_marker: char = '`';
    let mut fence_len: usize = 0;

    let mut headings: Vec<RawHeading> = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();

    for (idx, raw_line) in lines.iter().enumerate() {
        let line_num = idx + 1;
        let trimmed_start = raw_line.trim_start();

        if let Some((marker, count)) = detect_fence(trimmed_start) {
            if !in_fence {
                in_fence = true;
                fence_marker = marker;
                fence_len = count;
                continue;
            } else if marker == fence_marker
                && count >= fence_len
                && trimmed_start[count..].trim().is_empty()
            {
                in_fence = false;
                continue;
            }
        }

        if in_fence {
            continue;
        }

        if let Some((level, title)) = parse_atx_heading(trimmed_start) {
            if (1..=4).contains(&level) && !title.is_empty() {
                while let Some((top_lvl, _)) = stack.last() {
                    if *top_lvl >= level {
                        stack.pop();
                    } else {
                        break;
                    }
                }
                let parent_name = stack.last().map(|(_, t)| t.clone());
                let full_line = format!("{} {}", "#".repeat(level), title);
                stack.push((level, title.clone()));

                headings.push(RawHeading {
                    level,
                    title,
                    full_line,
                    line_num,
                    parent_name,
                });
            }
        }
    }

    let mut symbols = Vec::with_capacity(headings.len());
    let mut raw_sections = Vec::with_capacity(headings.len() + 1);

    if headings.is_empty() {
        push_section_with_windows(&mut raw_sections, "# Document", &lines, 1, total_lines);
        return ParsedMarkdown {
            symbols,
            sections: raw_sections,
        };
    }

    if headings[0].line_num > 1 {
        push_section_with_windows(
            &mut raw_sections,
            "# Preamble",
            &lines,
            1,
            headings[0].line_num - 1,
        );
    }

    for i in 0..headings.len() {
        let h = &headings[i];
        let start_line = h.line_num;
        let section_end = if i + 1 < headings.len() {
            headings[i + 1].line_num - 1
        } else {
            total_lines
        }
        .max(start_line);

        let _ = h.level;
        symbols.push(ExtractedSymbol {
            name: h.title.clone(),
            kind: "md_heading".to_string(),
            signature: h.full_line.clone(),
            start_line,
            end_line: section_end,
            parent_name: h.parent_name.clone(),
        });

        push_section_with_windows(
            &mut raw_sections,
            &h.full_line,
            &lines,
            start_line,
            section_end,
        );
    }

    let sections = coalesce_short_sections(raw_sections);
    ParsedMarkdown { symbols, sections }
}

fn coalesce_short_sections(raw: Vec<DocSectionChunk>) -> Vec<DocSectionChunk> {
    let mut out: Vec<DocSectionChunk> = Vec::with_capacity(raw.len());
    for sec in raw {
        if let Some(last) = out.last_mut() {
            let combined_lines = sec.end_line.saturating_sub(last.start_line) + 1;
            let combined_bytes = last.content.len() + 1 + sec.content.len();
            if combined_lines <= COALESCE_MAX_LINES && combined_bytes <= COALESCE_MAX_BYTES {
                last.end_line = sec.end_line;
                last.heading = format!("{} / {}", last.heading, sec.heading.trim_start_matches('#').trim());
                last.content.push('\n');
                last.content.push_str(&sec.content);
                continue;
            }
        }
        out.push(sec);
    }
    out
}

fn push_section_with_windows(
    sections: &mut Vec<DocSectionChunk>,
    heading: &str,
    lines: &[&str],
    start_line: usize,
    end_line: usize,
) {
    if lines.is_empty() || start_line > end_line || start_line > lines.len() {
        return;
    }
    let clamped_end = end_line.min(lines.len());
    let total_sec_lines = clamped_end - start_line + 1;

    if total_sec_lines <= SECTION_WINDOW_LINES {
        let content = lines[(start_line - 1)..clamped_end].join("\n").trim().to_string();
        if !content.is_empty() {
            sections.push(DocSectionChunk {
                heading: heading.to_string(),
                start_line,
                end_line: clamped_end,
                content,
            });
        }
        return;
    }

    let step = (SECTION_WINDOW_LINES - SECTION_OVERLAP_LINES).max(1);
    let mut win_start = start_line;
    let mut part = 1usize;

    while win_start <= clamped_end {
        let win_end = (win_start + SECTION_WINDOW_LINES - 1).min(clamped_end);
        let content = lines[(win_start - 1)..win_end].join("\n").trim().to_string();
        if !content.is_empty() {
            let win_heading = if part == 1 {
                heading.to_string()
            } else {
                format!("{heading} (part {part}, L{win_start}..{win_end})")
            };
            sections.push(DocSectionChunk {
                heading: win_heading,
                start_line: win_start,
                end_line: win_end,
                content,
            });
            part += 1;
        }
        if win_end >= clamped_end {
            break;
        }
        win_start += step;
    }
}

fn detect_fence(trimmed: &str) -> Option<(char, usize)> {
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = trimmed.chars().take_while(|&c| c == first).count();
    if count >= 3 {
        Some((first, count))
    } else {
        None
    }
}

fn parse_atx_heading(trimmed: &str) -> Option<(usize, String)> {
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim().to_string();
    Some((hashes, title))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_markdown_ignores_code_fences_and_extracts_sections() {
        let md = r#"# Kaye Architecture

Intro text.

```bash
# This bash comment must NOT be a heading
## Neither should this
kaye init
```

## Database Schema
Details about SQLite.

~~~python
# Python comment inside tilde fence
def foo():
    pass
~~~

### FTS5 Triggers
Trigger details here.
"#;

        let parsed = parse_markdown(md);
        assert_eq!(parsed.symbols.len(), 3);
        assert_eq!(parsed.symbols[0].name, "Kaye Architecture");
        assert_eq!(parsed.symbols[0].signature, "# Kaye Architecture");
        assert_eq!(parsed.symbols[0].start_line, 1);
        assert_eq!(parsed.symbols[0].end_line, 10);

        assert_eq!(parsed.symbols[1].name, "Database Schema");
        assert_eq!(parsed.symbols[1].signature, "## Database Schema");
        assert_eq!(
            parsed.symbols[1].parent_name.as_deref(),
            Some("Kaye Architecture")
        );
        assert_eq!(parsed.symbols[1].start_line, 11);
        assert_eq!(parsed.symbols[1].end_line, 19);

        assert_eq!(parsed.symbols[2].name, "FTS5 Triggers");
        assert_eq!(parsed.symbols[2].signature, "### FTS5 Triggers");
        assert_eq!(
            parsed.symbols[2].parent_name.as_deref(),
            Some("Database Schema")
        );
        assert_eq!(parsed.symbols[2].start_line, 20);
    }

    #[test]
    fn test_long_markdown_section_overlapping_windows() {
        let mut md = String::from("## Deep Guide\n");
        for i in 2..=260 {
            md.push_str(&format!("Line {i}: detailed explanation about architecture item {i}\n"));
        }
        let parsed = parse_markdown(&md);
        assert_eq!(parsed.symbols.len(), 1);
        assert_eq!(parsed.symbols[0].start_line, 1);
        assert_eq!(parsed.symbols[0].end_line, 260);

        // Windows of 120 lines with step 105 (1..120, 106..225, 211..260)
        assert_eq!(parsed.sections.len(), 3);
        assert_eq!(parsed.sections[0].start_line, 1);
        assert_eq!(parsed.sections[0].end_line, 120);
        assert_eq!(parsed.sections[1].start_line, 106);
        assert_eq!(parsed.sections[1].end_line, 225);
        assert_eq!(parsed.sections[2].start_line, 211);
        assert_eq!(parsed.sections[2].end_line, 260);
    }
}
