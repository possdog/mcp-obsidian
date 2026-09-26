//! The note service: read-side operations over the vault that combine the
//! sandboxed file layer ([`crate::vault`]) with Markdown parsing
//! ([`crate::markdown`]).
//!
//! Phase 4 binds these operations to MCP tools; keeping them here means the
//! protocol layer stays a thin wrapper and everything is testable without
//! a server.

use std::collections::HashMap;

use crate::markdown;
use crate::vault::{NoteEntry, Vault, VaultError};

pub type Result<T> = std::result::Result<T, VaultError>;

/// A note read by [`Notes::read`].
#[derive(Debug, Clone)]
pub struct ReadResult {
    pub frontmatter: Option<String>,
    /// The body: the full note body, or a single section when a heading
    /// was requested.
    pub body: String,
}

/// An outgoing link from a note, resolved to a vault-relative path.
#[derive(Debug, Clone, PartialEq)]
pub struct OutLink {
    /// Vault-relative path of the link target.
    pub target: String,
    /// What the link displays (wikilink alias or markdown link text).
    pub display: String,
    /// `true` for embeds (`![[…]]`, `![]()`).
    pub embed: bool,
}

/// A link pointing at a note from elsewhere in the vault.
#[derive(Debug, Clone, PartialEq)]
pub struct BackLink {
    /// Vault-relative path of the note containing the link.
    pub source: String,
    pub display: String,
}

/// The vault's link graph, built in one pass over all notes.
///
/// Built per request for now: a fresh index reflects every write
/// immediately without invalidation logic, and O(notes) scans are cheap
/// for typical vaults (see PLAN.md's open-items note).
#[derive(Debug, Default)]
pub struct LinkIndex {
    outlinks: HashMap<String, Vec<OutLink>>,
    backlinks: HashMap<String, Vec<BackLink>>,
}

impl LinkIndex {
    pub fn outlinks(&self, path: &str) -> &[OutLink] {
        self.outlinks.get(path).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn backlinks(&self, path: &str) -> &[BackLink] {
        self.backlinks
            .get(path)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}

/// A tag with the number of distinct notes carrying it.
#[derive(Debug, Clone, PartialEq)]
pub struct TagCount {
    pub tag: String,
    pub notes: usize,
}

/// Read-side note operations over a vault.
#[derive(Debug, Clone)]
pub struct Notes {
    vault: Vault,
}

impl Notes {
    pub fn open(vault: Vault) -> Self {
        Self { vault }
    }

    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    /// Lists notes (all files), optionally under a path prefix.
    pub fn list(&self, prefix: Option<&str>) -> Result<Vec<NoteEntry>> {
        self.vault.list(prefix)
    }

    /// Reads a note. With `heading`, only that section is returned;
    /// matching is case-insensitive for ergonomics.
    pub fn read(&self, path: &str, heading: Option<&str>) -> Result<ReadResult> {
        let raw_bytes = self.vault.read(path)?;
        // Vault notes are text; lossy conversion keeps reads working even
        // with a stray invalid byte instead of erroring the whole call.
        let raw = String::from_utf8_lossy(&raw_bytes).into_owned();
        let (frontmatter, body) = markdown::split_frontmatter(&raw);
        let body = match heading {
            Some(h) => markdown::extract_section(body, h)
                .ok_or_else(|| VaultError::NotFound(format!("heading {h:?} in note {path}")))?,
            None => body.to_string(),
        };
        Ok(ReadResult {
            frontmatter: frontmatter.map(str::to_string),
            body,
        })
    }

    /// Builds the whole-vault link index: for every Markdown note, parse
    /// wikilinks and markdown links and resolve them to vault paths.
    pub fn link_index(&self) -> Result<LinkIndex> {
        let entries = self.vault.list(None)?;
        let mut index = LinkIndex::default();

        // Resolution tables. Obsidian matches link targets
        // case-insensitively regardless of filesystem, so keys are
        // lowercased. Two tables: full paths, and basenames (filename with
        // extension; `.md` files additionally get a key without their
        // extension so `[[note]]` works).
        let mut by_path: HashMap<String, String> = HashMap::new();
        let mut by_basename: HashMap<String, Vec<String>> = HashMap::new();
        for entry in &entries {
            let path = &entry.path;
            by_path.insert(path.to_lowercase(), path.clone());
            let file_name = path.rsplit('/').next().unwrap_or(path);
            by_basename
                .entry(file_name.to_lowercase())
                .or_default()
                .push(path.clone());
            if let Some(stem) = file_name.strip_suffix(".md") {
                by_basename
                    .entry(stem.to_lowercase())
                    .or_default()
                    .push(path.clone());
            }
        }

        for entry in &entries {
            if !entry.path.ends_with(".md") {
                continue;
            }
            let raw = String::from_utf8_lossy(&self.vault.read(&entry.path)?).into_owned();
            let (_, body) = markdown::split_frontmatter(&raw);
            let source = &entry.path;
            let source_dir = match source.rsplit_once('/') {
                Some((dir, _)) => dir.to_string(),
                None => String::new(),
            };

            let mut outlinks: Vec<OutLink> = Vec::new();
            // Obsidian counts wikilinks inside frontmatter values as graph
            // edges (e.g. `attachment: "[[pic.webp]]"`), so wikilinks are
            // parsed over the whole raw note, not just the body. Markdown
            // links stay body-only: URLs are common in frontmatter and add
            // only external links, which the graph skips anyway.
            for link in markdown::parse_wikilinks(&raw) {
                if let Some(target) = self.resolve_wikilink(&link.target, &by_path, &by_basename) {
                    outlinks.push(OutLink {
                        target,
                        display: link.alias.unwrap_or_else(|| link.target.clone()),
                        embed: link.embed,
                    });
                }
            }
            for link in markdown::parse_md_links(body) {
                if link.external {
                    continue;
                }
                let decoded = markdown::percent_decode(&link.target);
                let joined = normalize_join(&source_dir, &decoded);
                // Markdown links are relative to the note, so resolve via
                // the full-path table only.
                if let Some(target) = by_path.get(&joined.to_lowercase()) {
                    outlinks.push(OutLink {
                        target: target.clone(),
                        display: if link.text.is_empty() {
                            link.target.clone()
                        } else {
                            link.text.clone()
                        },
                        embed: link.embed,
                    });
                }
            }

            if !outlinks.is_empty() {
                for out in &outlinks {
                    index
                        .backlinks
                        .entry(out.target.clone())
                        .or_default()
                        .push(BackLink {
                            source: source.clone(),
                            display: out.display.clone(),
                        });
                }
                index.outlinks.insert(source.clone(), outlinks);
            }
        }
        Ok(index)
    }

    /// Resolves one wikilink target against the tables, mirroring
    /// Obsidian's rules:
    /// - empty target = intra-note link, resolves to the note itself;
    /// - targets containing `/` are vault-root paths (with or without
    ///   `.md`, with extension required for attachments);
    /// - bare names match by filename anywhere in the vault;
    /// - ambiguity between same-named notes resolves to the shortest
    ///   path (Obsidian's "shortest path" resolution).
    fn resolve_wikilink(
        &self,
        target: &str,
        by_path: &HashMap<String, String>,
        by_basename: &HashMap<String, Vec<String>>,
    ) -> Option<String> {
        let target = target.trim();
        if target.is_empty() {
            return None; // intra-note links do not enter the graph
        }
        if target.contains('/') {
            let lower = target.to_lowercase();
            return by_path
                .get(&lower)
                .or_else(|| by_path.get(&format!("{lower}.md")))
                .cloned();
        }
        let candidates = by_basename.get(&target.to_lowercase())?;
        match candidates.len() {
            0 | 1 => candidates.first().cloned(),
            // Shortest path (fewest segments), lexicographic tiebreak for
            // deterministic output.
            _ => candidates
                .iter()
                .min_by_key(|p| (p.split('/').count(), p.to_string()))
                .cloned(),
        }
    }

    /// Tag inventory: every tag from frontmatter and body, with the number
    /// of distinct notes carrying it, sorted by count then name.
    pub fn list_tags(&self) -> Result<Vec<TagCount>> {
        let entries = self.vault.list(None)?;
        let mut counts: HashMap<String, usize> = HashMap::new();
        for entry in &entries {
            if !entry.path.ends_with(".md") {
                continue;
            }
            let raw = String::from_utf8_lossy(&self.vault.read(&entry.path)?).into_owned();
            let (frontmatter, body) = markdown::split_frontmatter(&raw);
            // One note can carry a tag in both frontmatter and body; count
            // the note once by collecting a per-note set first.
            let mut tags: std::collections::BTreeSet<String> =
                markdown::parse_body_tags(body).into_iter().collect();
            if let Some(value) = frontmatter.and_then(markdown::parse_yaml) {
                for tag in markdown::frontmatter_string_list(&value, "tags") {
                    tags.insert(tag);
                }
            }
            for tag in tags {
                *counts.entry(tag).or_default() += 1;
            }
        }
        let mut out: Vec<TagCount> = counts
            .into_iter()
            .map(|(tag, notes)| TagCount { tag, notes })
            .collect();
        out.sort_by(|a, b| b.notes.cmp(&a.notes).then(a.tag.cmp(&b.tag)));
        Ok(out)
    }
}

/// Joins a relative link target onto the source note's directory and
/// normalizes lexically. `..` is clamped at the vault root, so the result
/// always stays inside the vault (the graph never re-routes through the
/// sandboxing resolver, which would reject legitimate `../` links).
fn normalize_join(dir: &str, rel: &str) -> String {
    let combined = if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for segment in combined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::Vault;

    fn vault_with_notes(notes: &[(&str, &str)]) -> (Notes, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let vault = Vault::open(dir.path()).expect("vault opens");
        for (path, content) in notes {
            vault.write(path, content.as_bytes()).expect("write");
        }
        (Notes::open(vault), dir)
    }

    #[test]
    fn link_graph_resolves_wikilinks_md_links_and_embeds() {
        let (notes, _d) = vault_with_notes(&[
            ("A.md", "[[B]] [[Missing]] [c link](sub/C.md) ![[pic.png]]"),
            ("sub/B.md", "[[A|alias]]"),
            ("sub/C.md", "# C"),
            ("pic.png", "fake image bytes"),
        ]);
        let index = notes.link_index().expect("index builds");

        // Wikilinks are collected before markdown links, hence the order.
        let out_a: Vec<_> = index
            .outlinks("A.md")
            .iter()
            .map(|o| o.target.as_str())
            .collect();
        assert_eq!(out_a, ["sub/B.md", "pic.png", "sub/C.md"]);

        let back_b = index.backlinks("sub/B.md");
        assert_eq!(back_b.len(), 1);
        assert_eq!(back_b[0].source, "A.md");

        let out_b = index.outlinks("sub/B.md");
        assert_eq!(out_b[0].target, "A.md");
        assert_eq!(out_b[0].display, "alias");

        // C is reachable via markdown link, pic.png via embed.
        assert_eq!(index.backlinks("sub/C.md")[0].source, "A.md");
        assert!(
            index
                .outlinks("A.md")
                .iter()
                .any(|o| o.embed && o.target == "pic.png")
        );
        // Unresolved links appear nowhere.
        assert!(index.backlinks("Missing.md").is_empty());
    }

    #[test]
    fn link_resolution_case_insensitive_and_ambiguous() {
        let (notes, _d) = vault_with_notes(&[
            ("Target.md", "x"),
            ("a/long/path/Dup.md", "[[dup]]"),
            ("short/Dup.md", "x"),
            ("Linker.md", "[[target]] [[dup]]"),
        ]);
        let index = notes.link_index().expect("index builds");
        // Case-insensitive resolution
        assert_eq!(index.outlinks("Linker.md")[0].target, "Target.md");
        // Ambiguity resolved to shortest path
        let dup_links: Vec<_> = index
            .outlinks("Linker.md")
            .iter()
            .filter(|o| o.target.ends_with("Dup.md"))
            .map(|o| o.target.as_str())
            .collect();
        assert_eq!(dup_links, ["short/Dup.md"]);
    }

    #[test]
    fn md_links_relative_to_note_and_percent_encoded() {
        let (notes, _d) = vault_with_notes(&[
            ("sub/inner.md", "[up](../Top.md) [spaced](My%20Note.md)"),
            ("Top.md", "top"),
            ("sub/My Note.md", "spaced note"),
        ]);
        let index = notes.link_index().expect("index builds");
        let out: Vec<_> = index
            .outlinks("sub/inner.md")
            .iter()
            .map(|o| o.target.as_str())
            .collect();
        assert_eq!(out, ["Top.md", "sub/My Note.md"]);
    }

    #[test]
    fn frontmatter_wikilinks_count_as_edges() {
        let (notes, _d) = vault_with_notes(&[
            ("a.md", "---\nattachment: \"[[pic.webp]]\"\n---\nplain body"),
            ("pic.webp", "fake bytes"),
        ]);
        let index = notes.link_index().expect("index builds");
        assert_eq!(index.outlinks("a.md").len(), 1);
        assert_eq!(index.outlinks("a.md")[0].target, "pic.webp");
        assert_eq!(index.backlinks("pic.webp")[0].source, "a.md");
    }

    #[test]
    fn backlinks_exclude_code_blocks() {
        let (notes, _d) = vault_with_notes(&[
            ("Code.md", "```\n[[Real]]\n```\n`[[Real]]`"),
            ("Real.md", "real"),
        ]);
        let index = notes.link_index().expect("index builds");
        assert!(index.backlinks("Real.md").is_empty());
    }

    #[test]
    fn read_with_and_without_heading() {
        let (notes, _d) = vault_with_notes(&[(
            "n.md",
            "---\ntitle: N\n---\n# Top\ntop text\n## Section\nsection text\n## Two\n",
        )]);
        let full = notes.read("n.md", None).expect("read");
        assert_eq!(full.frontmatter.as_deref(), Some("---\ntitle: N\n---\n"));
        assert!(full.body.contains("# Top"));

        let section = notes.read("n.md", Some("section")).expect("section");
        assert_eq!(section.body, "## Section\nsection text");
        assert!(matches!(
            notes.read("n.md", Some("nope")),
            Err(VaultError::NotFound(_))
        ));
    }

    #[test]
    fn tag_inventory_counts_notes_once() {
        let (notes, _d) = vault_with_notes(&[
            (
                "a.md",
                "---\ntags: [alpha, shared]\n---\n#alpha in body too",
            ),
            ("b.md", "#shared #alpha #beta"),
            ("c.md", "# Heading not a tag\n#beta"),
        ]);
        let tags = notes.list_tags().expect("tags");
        let get = |t: &str| {
            tags.iter()
                .find(|c| c.tag == t)
                .map(|c| c.notes)
                .unwrap_or(0)
        };
        assert_eq!(get("alpha"), 2, "a via frontmatter, b via body");
        assert_eq!(get("shared"), 2, "a frontmatter + b body");
        assert_eq!(get("beta"), 2);
        // Frontmatter + body in the same note counts once: alpha=2, not 3.
        assert_eq!(tags[0].notes, 2);
    }
}
