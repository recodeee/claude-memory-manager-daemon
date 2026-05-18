//! Cheap deterministic audit of MEMORY_ROOT.
//!
//! This is the FIRST thing a tick does. If `total_issues() == 0`, the daemon
//! skips the `claude -p` spawn entirely — saving 30-80k tokens and ~30-60s
//! per tick. The agent only gets called when there's something the Rust
//! checks can't decide on their own (duplicate fact merges, prose quality,
//! reorganization, judgment calls).

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Default)]
pub struct AuditReport {
    pub root: String,
    pub file_count: u64,
    pub memory_md_lines: u64,
    pub memory_md_oversize: bool,
    /// Files that have *.md extension but no parseable YAML frontmatter.
    pub missing_frontmatter: Vec<String>,
    /// Files whose frontmatter declares `metadata.type` outside the four valid values.
    pub invalid_type: Vec<TypeIssue>,
    /// MEMORY.md lines that point to files that don't exist.
    pub dangling_index_entries: Vec<String>,
    /// *.md files (excluding MEMORY.md) that exist on disk but no MEMORY.md line refs them.
    pub missing_from_index: Vec<String>,
    /// `[[wikilink]]` references whose target slug isn't a known `name:` in another file.
    pub broken_wikilinks: Vec<WikilinkIssue>,
    /// Feedback or project entries missing **Why:** or **How to apply:** lines.
    pub missing_why_or_how: Vec<String>,
    /// Pairs of files whose descriptions match — likely the same fact recorded twice.
    pub duplicate_candidates: Vec<DupePair>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TypeIssue {
    pub file: String,
    pub got: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WikilinkIssue {
    pub from: String,
    pub to_slug: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DupePair {
    pub a: String,
    pub b: String,
    pub reason: String,
}

impl AuditReport {
    pub fn total_issues(&self) -> usize {
        self.missing_frontmatter.len()
            + self.invalid_type.len()
            + self.dangling_index_entries.len()
            + self.missing_from_index.len()
            + self.broken_wikilinks.len()
            + self.missing_why_or_how.len()
            + self.duplicate_candidates.len()
            + (self.memory_md_oversize as usize)
    }

    /// Concise one-line summary, suitable for log lines.
    pub fn summary(&self) -> String {
        format!(
            "files={} mem_md_lines={}{} issues={}: \
             missing_fm={} bad_type={} dangling={} unindexed={} broken_links={} \
             missing_why={} dupes={}",
            self.file_count,
            self.memory_md_lines,
            if self.memory_md_oversize {
                "(OVERSIZE)"
            } else {
                ""
            },
            self.total_issues(),
            self.missing_frontmatter.len(),
            self.invalid_type.len(),
            self.dangling_index_entries.len(),
            self.missing_from_index.len(),
            self.broken_wikilinks.len(),
            self.missing_why_or_how.len(),
            self.duplicate_candidates.len(),
        )
    }
}

const VALID_TYPES: &[&str] = &["user", "feedback", "project", "reference"];
const MEMORY_MD_LINE_BUDGET: u64 = 200;

#[derive(Debug, Default)]
struct ParsedFile {
    filename: String,
    name_slug: Option<String>,
    description: Option<String>,
    declared_type: Option<String>,
    body: String,
    has_frontmatter: bool,
}

pub fn run_audit(root: &Path) -> AuditReport {
    let mut report = AuditReport {
        root: root.display().to_string(),
        ..Default::default()
    };
    if !root.is_dir() {
        return report;
    }

    // Collect every *.md file at the top level of MEMORY_ROOT.
    // We deliberately don't recurse — memory layout is flat by design.
    let mut files: Vec<ParsedFile> = Vec::new();
    let mut memory_md_present = false;
    let mut memory_md_lines: Vec<String> = Vec::new();

    for entry in WalkDir::new(root).max_depth(1).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".md") {
            continue;
        }
        let path = entry.path();
        let content = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if name == "MEMORY.md" {
            memory_md_present = true;
            memory_md_lines = content.lines().map(|s| s.to_string()).collect();
            report.memory_md_lines = memory_md_lines.len() as u64;
            report.memory_md_oversize = report.memory_md_lines > MEMORY_MD_LINE_BUDGET;
            continue;
        }
        files.push(parse_file(&name, &content));
    }
    report.file_count = files.len() as u64;

    // 1. Per-file checks.
    let mut name_to_filename: HashMap<String, String> = HashMap::new();
    for f in &files {
        if !f.has_frontmatter || f.name_slug.is_none() {
            report.missing_frontmatter.push(f.filename.clone());
            continue;
        }
        if let Some(slug) = &f.name_slug {
            name_to_filename.insert(slug.clone(), f.filename.clone());
        }
        if let Some(t) = &f.declared_type {
            if !VALID_TYPES.contains(&t.as_str()) {
                report.invalid_type.push(TypeIssue {
                    file: f.filename.clone(),
                    got: t.clone(),
                });
            }
            // feedback / project entries should carry the structured lines.
            if matches!(t.as_str(), "feedback" | "project")
                && (!body_has_why(&f.body) || !body_has_how(&f.body))
            {
                report.missing_why_or_how.push(f.filename.clone());
            }
        }
    }

    // 2. MEMORY.md cross-check.
    if memory_md_present {
        let mut index_targets: HashSet<String> = HashSet::new();
        let on_disk: HashSet<String> = files.iter().map(|f| f.filename.clone()).collect();
        for line in &memory_md_lines {
            if let Some(target) = extract_md_link_target(line) {
                index_targets.insert(target.clone());
                if !on_disk.contains(&target) {
                    report.dangling_index_entries.push(target);
                }
            }
        }
        for f in &files {
            if !index_targets.contains(&f.filename) {
                report.missing_from_index.push(f.filename.clone());
            }
        }
    }

    // 3. Wikilink integrity.
    for f in &files {
        for slug in extract_wikilinks(&f.body) {
            if !name_to_filename.contains_key(&slug) {
                report.broken_wikilinks.push(WikilinkIssue {
                    from: f.filename.clone(),
                    to_slug: slug,
                });
            }
        }
    }

    // 4. Duplicate description detection — same `description:` text exactly.
    let mut desc_to_files: HashMap<String, Vec<String>> = HashMap::new();
    for f in &files {
        if let Some(desc) = &f.description {
            let key = normalize_desc(desc);
            if key.len() >= 16 {
                desc_to_files
                    .entry(key)
                    .or_default()
                    .push(f.filename.clone());
            }
        }
    }
    for (_desc, group) in desc_to_files {
        if group.len() > 1 {
            for i in 0..group.len() {
                for j in (i + 1)..group.len() {
                    report.duplicate_candidates.push(DupePair {
                        a: group[i].clone(),
                        b: group[j].clone(),
                        reason: "matching description text".to_string(),
                    });
                }
            }
        }
    }

    report
}

fn parse_file(filename: &str, content: &str) -> ParsedFile {
    let mut pf = ParsedFile {
        filename: filename.to_string(),
        ..Default::default()
    };
    let (frontmatter, body) = match split_frontmatter(content) {
        Some(x) => x,
        None => {
            pf.body = content.to_string();
            return pf;
        }
    };
    pf.has_frontmatter = true;
    pf.body = body.to_string();

    // Naive YAML scan — enough for {name, description, metadata.type}.
    let mut in_metadata = false;
    for raw in frontmatter.lines() {
        let line = raw.trim_end();
        if line.starts_with("metadata:") {
            in_metadata = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("name:") {
            pf.name_slug = trim_yaml_value(rest);
            in_metadata = false;
        } else if let Some(rest) = line.strip_prefix("description:") {
            pf.description = trim_yaml_value(rest);
            in_metadata = false;
        } else if in_metadata {
            if let Some(rest) = line.trim_start().strip_prefix("type:") {
                pf.declared_type = trim_yaml_value(rest);
            }
        }
    }
    pf
}

fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let s = content.strip_prefix("---\n")?;
    let end = s.find("\n---\n")?;
    let frontmatter = &s[..end];
    let body = &s[end + 5..];
    Some((frontmatter, body))
}

fn trim_yaml_value(s: &str) -> Option<String> {
    let v = s.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

fn extract_md_link_target(line: &str) -> Option<String> {
    // Markdown link form: "- [title](target.md) — ..."
    let open = line.find("](")?;
    let after = &line[open + 2..];
    let close = after.find(')')?;
    let target = &after[..close];
    if target.ends_with(".md") {
        Some(target.to_string())
    } else {
        None
    }
}

fn extract_wikilinks(body: &str) -> Vec<String> {
    // Skip wikilinks that appear inside fenced ``` code blocks or inline `code`
    // spans — they're example syntax, not real refs. This kills the audit
    // false-positive class observed in protocol_memory_usage.md (`[[name]]`,
    // `[[links]]` appearing as documentation).
    let mut out = Vec::new();
    let mut in_fenced = false;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fenced = !in_fenced;
            continue;
        }
        if in_fenced {
            continue;
        }
        // Strip inline backtick spans before scanning.
        let stripped = strip_inline_code(line);
        let mut rest = stripped.as_str();
        while let Some(idx) = rest.find("[[") {
            let after = &rest[idx + 2..];
            if let Some(close) = after.find("]]") {
                let slug = after[..close].trim().to_string();
                if !slug.is_empty() {
                    out.push(slug);
                }
                rest = &after[close + 2..];
            } else {
                break;
            }
        }
    }
    out
}

fn strip_inline_code(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_code = false;
    for ch in line.chars() {
        if ch == '`' {
            in_code = !in_code;
            continue;
        }
        if !in_code {
            out.push(ch);
        }
    }
    out
}

fn body_has_why(body: &str) -> bool {
    body.contains("**Why:**") || body.contains("**Why**:")
}
fn body_has_how(body: &str) -> bool {
    body.contains("**How to apply:**") || body.contains("**How to apply**:")
}

fn normalize_desc(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("cmmd-audit-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_md(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn flags_oversize_memory_md() {
        let dir = write_fixture("oversize");
        let big = (0..250)
            .map(|i| format!("- line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        write_md(&dir, "MEMORY.md", &big);
        let r = run_audit(&dir);
        assert!(r.memory_md_oversize);
        assert!(r.memory_md_lines >= 250);
    }

    #[test]
    fn detects_missing_frontmatter() {
        let dir = write_fixture("missing-fm");
        write_md(&dir, "a.md", "no frontmatter here\n");
        let r = run_audit(&dir);
        assert_eq!(r.missing_frontmatter, vec!["a.md".to_string()]);
    }

    #[test]
    fn detects_dangling_memory_md_entry() {
        let dir = write_fixture("dangling");
        write_md(&dir, "MEMORY.md", "- [Missing](nope.md) — gone\n");
        let r = run_audit(&dir);
        assert!(r.dangling_index_entries.contains(&"nope.md".to_string()));
    }

    #[test]
    fn detects_unindexed_file() {
        let dir = write_fixture("unindexed");
        write_md(&dir, "MEMORY.md", "- [Other](other.md) — present\n");
        write_md(
            &dir,
            "other.md",
            "---\nname: other\ndescription: x\nmetadata:\n  type: user\n---\n\nbody\n",
        );
        write_md(
            &dir,
            "orphan.md",
            "---\nname: orphan\ndescription: y\nmetadata:\n  type: user\n---\n\nbody\n",
        );
        let r = run_audit(&dir);
        assert!(r.missing_from_index.contains(&"orphan.md".to_string()));
        assert!(!r.missing_from_index.contains(&"other.md".to_string()));
    }

    #[test]
    fn detects_invalid_type() {
        let dir = write_fixture("invalid-type");
        write_md(
            &dir,
            "a.md",
            "---\nname: a\ndescription: x\nmetadata:\n  type: notathing\n---\n\nbody\n",
        );
        let r = run_audit(&dir);
        assert!(r.invalid_type.iter().any(|t| t.got == "notathing"));
    }

    #[test]
    fn feedback_without_why_is_flagged() {
        let dir = write_fixture("no-why");
        write_md(
            &dir,
            "a.md",
            "---\nname: a\ndescription: x\nmetadata:\n  type: feedback\n---\n\nNo why, no how.\n",
        );
        let r = run_audit(&dir);
        assert!(r.missing_why_or_how.contains(&"a.md".to_string()));
    }

    #[test]
    fn duplicate_descriptions_detected() {
        let dir = write_fixture("dupes");
        let body = "---\nname: {n}\ndescription: auth middleware rewrite is compliance driven\nmetadata:\n  type: project\n---\n\nbody\n";
        write_md(&dir, "a.md", &body.replace("{n}", "a"));
        write_md(&dir, "b.md", &body.replace("{n}", "b"));
        let r = run_audit(&dir);
        assert!(
            !r.duplicate_candidates.is_empty(),
            "should flag matching descriptions"
        );
    }

    #[test]
    fn wikilinks_inside_fenced_code_are_ignored() {
        let dir = write_fixture("fenced");
        write_md(&dir, "MEMORY.md", "- [Doc](doc.md) — protocol notes\n");
        let body = "---\nname: doc\ndescription: x\nmetadata:\n  type: reference\n---\n\n\
            Real link: [[real-name]]\n\n\
            ```\n\
            Example syntax: [[name]] and [[links]]\n\
            ```\n\n\
            Inline `[[also-skipped]]` example.\n";
        write_md(&dir, "doc.md", body);
        // Also create a real target so [[real-name]] resolves.
        write_md(
            &dir,
            "real.md",
            "---\nname: real-name\ndescription: y\nmetadata:\n  type: reference\n---\n\nbody\n",
        );
        let r = run_audit(&dir);
        let broken_slugs: Vec<&str> = r
            .broken_wikilinks
            .iter()
            .map(|w| w.to_slug.as_str())
            .collect();
        assert!(
            !broken_slugs.contains(&"name"),
            "[[name]] inside ``` must be skipped, got {broken_slugs:?}"
        );
        assert!(
            !broken_slugs.contains(&"links"),
            "[[links]] inside ``` must be skipped"
        );
        assert!(
            !broken_slugs.contains(&"also-skipped"),
            "[[also-skipped]] inside inline `code` must be skipped"
        );
    }

    #[test]
    fn clean_dir_reports_zero_issues() {
        let dir = write_fixture("clean");
        write_md(&dir, "MEMORY.md", "- [Foo](foo.md) — clean entry\n");
        write_md(
            &dir,
            "foo.md",
            "---\nname: foo\ndescription: clean\nmetadata:\n  type: user\n---\n\nbody\n",
        );
        let r = run_audit(&dir);
        assert_eq!(r.total_issues(), 0, "got: {}", r.summary());
    }
}
