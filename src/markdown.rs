//! Obsidian-flavored Markdown parsing: frontmatter, wikilinks, markdown
//! links, tags, and headings.
//!
//! The parsers are read-only but return *byte spans* for every token, so
//! later phases can rewrite links in place without re-deriving offsets.
//! Parsers operate on a copy of the text with code blocks blanked out
//! (see [`blank_code`]), which keeps spans aligned with the original text
//! while preventing links or tags inside code from producing phantom
//! graph edges.

use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;
use serde_yaml_ng::Value;

static WIKILINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(!?)\[\[([^\[\]\n]+)\]\]").expect("static regex is valid"));
static MD_LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(!?)\[([^\[\]\n]*)\]\(([^)\n]*)\)").expect("static regex is valid")
});
static TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[^\p{L}\p{N}_#/])#([\p{L}\p{N}_][\p{L}\p{N}_\-/]*)")
        .expect("static regex is valid")
});

/// Splits a note into its YAML frontmatter (verbatim, delimiters included)
/// and its body.
///
/// The opening fence must be the first line of the file, exactly `---`.
/// An unclosed fence is treated as no frontmatter at all rather than
/// letting the whole note be misread as one giant YAML block.
pub fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    if !(raw.starts_with("---\n") || raw.starts_with("---\r\n") || raw == "---") {
        return (None, raw);
    }
    let mut offset = 0usize;
    for line in raw.split_inclusive('\n') {
        if offset > 0 && line.trim_end_matches(['\r', '\n']) == "---" {
            let fm_end = offset + line.len();
            return (Some(&raw[..fm_end]), &raw[fm_end..]);
        }
        offset += line.len();
    }
    (None, raw)
}

/// Parses frontmatter text (delimiters included) into a YAML value.
/// Malformed YAML yields `None`: the raw text is still preserved by
/// callers, only structured access (tags, aliases) is lost.
///
/// The `---` fence lines are stripped before parsing: the trailing fence
/// would otherwise be read by libyaml as the start of a second document,
/// which serde rejects outright.
pub fn parse_yaml(frontmatter: &str) -> Option<Value> {
    let mut lines = frontmatter.lines();
    lines.next()?; // opening fence
    let mut content: Vec<&str> = lines.collect();
    if content.last().is_some_and(|last| last.trim() == "---") {
        content.pop(); // closing fence
    }
    serde_yaml_ng::from_str(&content.join("\n")).ok()
}

/// Extracts the values of a frontmatter field that can appear as a single
/// value or a list (Obsidian's `tags`, `aliases`, …).
///
/// A bare string is split on commas, matching Obsidian's behavior where
/// `tags: a, b` is equivalent to a list. Numeric and boolean values are
/// stringified so `tags: [1, two]` still yields something usable.
pub fn frontmatter_string_list(value: &Value, key: &str) -> Vec<String> {
    // A missing key yields an empty list rather than an error: notes are
    // free to omit tags/aliases entirely.
    let Some(field) = value.get(key) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match field {
        Value::String(s) => out.extend(
            s.split(',')
                .map(|item| item.trim().trim_start_matches('#').to_string()),
        ),
        Value::Sequence(seq) => {
            for item in seq {
                match item {
                    Value::String(s) => out.push(s.trim().trim_start_matches('#').to_string()),
                    Value::Number(n) => out.push(n.to_string()),
                    Value::Bool(b) => out.push(b.to_string()),
                    _ => {}
                }
            }
        }
        Value::Number(n) => out.push(n.to_string()),
        Value::Bool(b) => out.push(b.to_string()),
        _ => {}
    }
    out.retain(|s| !s.is_empty());
    out
}

/// A `[[wikilink]]` or embed `![[note]]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Wikilink {
    /// `true` for `![[…]]` embeds.
    pub embed: bool,
    /// Target as written, with heading/block reference stripped.
    /// May be empty for intra-note links (`[[#heading]]`).
    pub target: String,
    pub alias: Option<String>,
    pub heading: Option<String>,
    /// Block reference (`[[note#^blockid]]`).
    pub block: Option<String>,
    /// Byte span of the whole token (including `!` and brackets) in the
    /// original text.
    pub span: Range<usize>,
}

/// Parses all wikilinks in a body. Code blocks are ignored.
pub fn parse_wikilinks(body: &str) -> Vec<Wikilink> {
    let blanked = blank_code(body);
    WIKILINK_RE
        .captures_iter(&blanked)
        .map(|caps| {
            let embed = &caps[1] == "!";
            let inner = &caps[2];
            let span = caps.get(0).expect("whole match").range();
            let (target_part, alias) = match inner.split_once('|') {
                Some((target, alias)) => (target, Some(alias.to_string())),
                None => (inner, None),
            };
            // A target may carry a heading and/or block reference:
            // `note`, `note#Heading`, `note#^block`, `note#H#^block`.
            // Split on the *unowned* target_part to sidestep borrow
            // conflicts with the owned `target` below.
            let mut target = target_part.trim().to_string();
            let mut heading = None;
            let mut block = None;
            if let Some((path_part, reference)) = target_part.trim().split_once('#') {
                target = path_part.trim().to_string();
                if let Some((heading_part, block_part)) = reference.split_once("#^") {
                    heading = Some(heading_part.to_string());
                    block = Some(block_part.to_string());
                } else if let Some(block_part) = reference.strip_prefix('^') {
                    block = Some(block_part.to_string());
                } else {
                    heading = Some(reference.to_string());
                }
            }
            Wikilink {
                embed,
                target,
                alias,
                heading,
                block,
                span,
            }
        })
        .collect()
}

/// A standard markdown link `[text](target)` or image `![alt](target)`.
#[derive(Debug, Clone, PartialEq)]
pub struct MdLink {
    /// `true` for images `![alt](target)`.
    pub embed: bool,
    pub text: String,
    /// Target as written, anchor (`#…`) stripped, NOT yet percent-decoded
    /// or resolved — that needs the containing note's directory.
    pub target: String,
    /// External targets (http, mailto, …) are reported but should not
    /// enter the link graph.
    pub external: bool,
    pub span: Range<usize>,
}

/// Parses all markdown links in a body. Code blocks are ignored.
pub fn parse_md_links(body: &str) -> Vec<MdLink> {
    let blanked = blank_code(body);
    MD_LINK_RE
        .captures_iter(&blanked)
        .map(|caps| {
            let span = caps.get(0).expect("whole match").range();
            let raw_target = &caps[3];
            // Strip a trailing anchor; the graph only cares about files.
            let anchorless = raw_target.split('#').next().unwrap_or(raw_target);
            // Anything with a URL scheme is outside the vault. Checking for
            // "://" plus mailto keeps relative paths containing ':' working.
            let external = anchorless.contains("://") || anchorless.starts_with("mailto:");
            MdLink {
                embed: &caps[1] == "!",
                text: caps[2].to_string(),
                target: anchorless.to_string(),
                external,
                span,
            }
        })
        .collect()
}

/// Parses `#tags` from a note body (frontmatter must be removed first).
/// Heading lines (`# Heading`) and code blocks are excluded.
///
/// Obsidian tag rules applied here: a tag starts at `#` preceded by a
/// boundary (line start or a non-word character), may contain letters,
/// numbers, `_`, `-`, and `/` (nesting), cannot be only digits, and
/// trailing `-`/`/` are invalid.
pub fn parse_body_tags(body: &str) -> Vec<String> {
    let blanked = blank_code(body);
    let mut tags = Vec::new();
    for line in blanked.lines() {
        for caps in TAG_RE.captures_iter(line) {
            let tag = &caps[1];
            if tag.chars().all(|c| c.is_numeric()) {
                continue;
            }
            let trimmed = tag.trim_end_matches(['-', '/']);
            if !trimmed.is_empty() {
                tags.push(trimmed.to_string());
            }
        }
    }
    tags
}

/// Blanks out fenced code blocks and inline code spans, replacing their
/// content with spaces.
///
/// The output has exactly the same length as the input, so byte spans
/// measured on it are valid in the original text. Fences are ``` or ~~~
/// runs at a line start; inline spans are backtick-delimited regions.
pub fn blank_code(raw: &str) -> String {
    let mut out = raw.as_bytes().to_vec();
    let mut in_fence = false;
    let mut fence_marker: Vec<u8> = Vec::new();

    let mut line_start = 0usize;
    for line in raw.split_inclusive('\n') {
        let trimmed_start = line.trim_start();
        let is_fence_line = trimmed_start.starts_with("```") || trimmed_start.starts_with("~~~");
        if is_fence_line {
            let marker: Vec<u8> = trimmed_start
                .chars()
                .take_while(|c| *c == '`' || *c == '~')
                .map(|c| c as u8)
                .collect();
            if !in_fence {
                in_fence = true;
                fence_marker = marker.clone();
                // Opening fence line is blanked too: it is code context.
                blank_range(&mut out, line_start, line_start + line.len());
            } else if marker == fence_marker {
                in_fence = false;
                fence_marker.clear();
                blank_range(&mut out, line_start, line_start + line.len());
            }
        } else if in_fence {
            blank_range(&mut out, line_start, line_start + line.len());
        } else {
            blank_inline_code(&mut out, line_start, line_start + line.len());
        }
        line_start += line.len();
    }
    String::from_utf8(out).expect("byte-for-byte replacement keeps UTF-8 valid")
}

/// Blanks `[start, end)` in the buffer with spaces (newlines preserved so
/// line structure survives).
fn blank_range(buf: &mut [u8], start: usize, end: usize) {
    // Clamp before taking the mutable borrow of the slice.
    let start = start.min(buf.len());
    let end = end.min(buf.len());
    for byte in &mut buf[start..end] {
        if *byte != b'\n' && *byte != b'\r' {
            *byte = b' ';
        }
    }
}

/// Blanks inline `` `code` `` spans within a single line, leaving the
/// delimiting backticks in place. Operates on absolute buffer indices so
/// no borrow of the buffer is held across the blanking mutation.
fn blank_inline_code(buf: &mut [u8], start: usize, end: usize) {
    let mut i = start;
    while i < end {
        if buf[i] != b'`' {
            i += 1;
            continue;
        }
        let run_start = i;
        while i < end && buf[i] == b'`' {
            i += 1;
        }
        let run_len = i - run_start;
        // The closer is the next run of the SAME length anywhere later in
        // the line (Commonmark rule), not necessarily adjacent.
        let mut j = i;
        let mut closed = false;
        while j < end {
            if buf[j] == b'`' {
                let close_start = j;
                while j < end && buf[j] == b'`' {
                    j += 1;
                }
                if j - close_start == run_len {
                    blank_range(buf, run_start + run_len, close_start);
                    i = j;
                    closed = true;
                    break;
                }
            } else {
                j += 1;
            }
        }
        // An unclosed run is literal text; keep scanning after it.
        if !closed && i < end {
            i += 1;
        }
    }
}

/// A markdown heading (`#` through `######`).
#[derive(Debug, Clone, PartialEq)]
pub struct Heading {
    pub level: usize,
    pub text: String,
    /// Byte span of the heading line in the body.
    pub span: Range<usize>,
}

/// Parses headings from a note body.
pub fn parse_headings(body: &str) -> Vec<Heading> {
    let blanked = blank_code(body);
    let mut headings = Vec::new();
    let mut offset = 0usize;
    for line in blanked.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let level = rest.chars().take_while(|c| *c == '#').count() + 1;
            let rest = &rest[level - 1..];
            // A heading needs whitespace after the hashes; `#^blockid`
            // lines are block references, not headings.
            if level <= 6 && rest.starts_with([' ', '\t']) {
                headings.push(Heading {
                    level,
                    text: rest.trim().to_string(),
                    span: offset..offset + line.len(),
                });
            }
        }
        offset += line.len();
    }
    headings
}

/// Extracts the section under a heading: the heading line plus all content
/// up to the next heading of the same or higher level. Matching is
/// case-insensitive for tool ergonomics (Obsidian itself is
/// case-sensitive; a tool user should not have to guess casing).
pub fn extract_section(body: &str, heading: &str) -> Option<String> {
    let headings = parse_headings(body);
    let found = headings
        .iter()
        .find(|h| h.text.eq_ignore_ascii_case(heading.trim()))?;
    let mut end = body.len();
    for next in headings.iter().skip_while(|h| h.span != found.span) {
        if next.span == found.span {
            continue;
        }
        if next.level <= found.level {
            end = next.span.start;
            break;
        }
    }
    // Trim trailing newlines so callers get the section content only.
    let section = &body[found.span.start..end];
    Some(section.trim_end().to_string())
}

/// Percent-decodes a link target (Obsidian URL-encodes spaces and special
/// characters in markdown links, e.g. `My%20Note.md`).
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_frontmatter_variants() {
        let raw = "---\ntitle: A\n---\n# Body\n";
        let (fm, body) = split_frontmatter(raw);
        assert_eq!(fm.unwrap(), "---\ntitle: A\n---\n");
        assert_eq!(body, "# Body\n");

        // No frontmatter
        let (fm, body) = split_frontmatter("Just text\n---\nmore");
        assert!(fm.is_none());
        assert_eq!(body, "Just text\n---\nmore");

        // Unclosed fence: whole text is body
        let (fm, body) = split_frontmatter("---\ntitle: A\nno close");
        assert!(fm.is_none());
        assert_eq!(body, "---\ntitle: A\nno close");

        // Horizontal rule at line 1 is not frontmatter
        let (fm, _) = split_frontmatter("----\ntext");
        assert!(fm.is_none());
    }

    #[test]
    fn wikilink_variants() {
        let body = "[[Foo]] [[Bar|alias]] [[Baz#Heading]] [[Qux#^block]] [[Qux#H#^b]] \
                    ![[img.png]] [[#self]] [[dir/Sub]]";
        let links = parse_wikilinks(body);
        let targets: Vec<&str> = links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(
            targets,
            ["Foo", "Bar", "Baz", "Qux", "Qux", "img.png", "", "dir/Sub"]
        );
        assert_eq!(&body[links[0].span.clone()], "[[Foo]]");
        assert_eq!(links[1].alias.as_deref(), Some("alias"));
        assert_eq!(links[2].heading.as_deref(), Some("Heading"));
        assert_eq!(links[3].block.as_deref(), Some("block"));
        assert_eq!(links[4].heading.as_deref(), Some("H"));
        assert_eq!(links[4].block.as_deref(), Some("b"));
        assert!(links[5].embed);
        assert!(!links[0].embed);
    }

    #[test]
    fn md_link_variants() {
        let body = "[text](note.md) [ext](https://x.dev/a) [mail](mailto:a@b.c) \
                    ![pic](img.png) [anchor](note.md#sec) [sp](My%20Note.md) [](empty.md)";
        let links = parse_md_links(body);
        assert_eq!(links[0].target, "note.md");
        assert!(!links[0].external);
        assert!(links[1].external && links[2].external);
        assert!(links[3].embed);
        assert_eq!(links[4].target, "note.md");
        assert_eq!(links[6].target, "empty.md");
        assert_eq!(&body[links[0].span.clone()], "[text](note.md)");
    }

    #[test]
    fn tags_rules() {
        let body = "# Heading not a tag\n#real-tag text #nested/tag #2026\n\
                    `#incode` stays\n```\n#fenced\n```\nfoo#notatag ##northis #trail-";
        let tags = parse_body_tags(body);
        assert_eq!(tags, ["real-tag", "nested/tag", "trail"]);
    }

    #[test]
    fn frontmatter_tags_shapes() {
        let list = parse_yaml("---\ntags:\n  - a\n  - b\n---\n").unwrap();
        assert_eq!(frontmatter_string_list(&list, "tags"), ["a", "b"]);
        let csv = parse_yaml("---\ntags: x, y\n---\n").unwrap();
        assert_eq!(frontmatter_string_list(&csv, "tags"), ["x", "y"]);
        let single = parse_yaml("---\ntag: solo\n---\n").unwrap();
        assert_eq!(frontmatter_string_list(&single, "tag"), ["solo"]);
        let mixed = parse_yaml("---\ntags: [1, two, \"#hashed\"]\n---\n").unwrap();
        assert_eq!(
            frontmatter_string_list(&mixed, "tags"),
            ["1", "two", "hashed"]
        );
        let none = parse_yaml("---\nother: x\n---\n").unwrap();
        assert!(frontmatter_string_list(&none, "tags").is_empty());
    }

    #[test]
    fn headings_and_sections() {
        let body = "# Top\npara\n## Section\ninside\n### Deep\ndeep text\n## Other\nother\n";
        let headings = parse_headings(body);
        assert_eq!(
            headings.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(),
            ["Top", "Section", "Deep", "Other"]
        );
        let section = extract_section(body, "section").unwrap();
        assert_eq!(section, "## Section\ninside\n### Deep\ndeep text");

        // A level-1 section extends to the next level-1 heading, so both
        // level-2 sections belong to "Top" (outline semantics).
        let top = extract_section(body, "Top").unwrap();
        assert!(top.contains("## Section"));
        assert!(top.contains("## Other"));
        assert!(extract_section(body, "Missing").is_none());
    }

    #[test]
    fn blank_code_preserves_offsets() {
        let raw = "a `[[code]]` b\n```\n[[fenced]]\n```\n[[real]]";
        let blanked = blank_code(raw);
        assert_eq!(blanked.len(), raw.len());
        let links = parse_wikilinks(raw);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "real");
        assert_eq!(&raw[links[0].span.clone()], "[[real]]");
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("My%20Note.md"), "My Note.md");
        assert_eq!(percent_decode("plain.md"), "plain.md");
        assert_eq!(percent_decode("bad%2"), "bad%2");
    }
}
