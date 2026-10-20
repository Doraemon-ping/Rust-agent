//! Skill loading (s07).
//!
//! The system prompt carries a *catalog* of skill names and one-line
//! descriptions; the model reads a full `SKILL.md` only when it calls the
//! `load_skill` tool.
//!
//! ```text
//!   skills/<name>/SKILL.md  -->  SkillLoader  -->  system prompt catalog
//!                                     ^
//!   LLM -- load_skill(name) --> full SKILL.md -- tool_result --> LLM
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Manifest file looked for inside every skill directory.
pub const SKILL_MANIFEST: &str = "SKILL.md";

/// One discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Name the model loads the skill by: frontmatter `name`, else the directory.
    pub name: String,
    /// One-line summary shown in the catalog.
    pub description: String,
    /// The full manifest, returned by [`SkillLoader::load`].
    pub content: String,
    /// The manifest path this skill was read from.
    pub path: PathBuf,
}

/// Scans a directory of `*/SKILL.md` manifests.
#[derive(Debug, Clone, Default)]
pub struct SkillLoader {
    skills_dir: PathBuf,
    skills: BTreeMap<String, Skill>,
}

impl SkillLoader {
    /// Scan `skills_dir` for `*/SKILL.md`.
    ///
    /// A missing directory is not an error: the loader is simply empty.
    pub fn new(skills_dir: impl Into<PathBuf>) -> Self {
        let mut loader = SkillLoader {
            skills_dir: skills_dir.into(),
            skills: BTreeMap::new(),
        };
        loader.scan();
        loader
    }

    /// The directory being scanned.
    pub fn dir(&self) -> &Path {
        &self.skills_dir
    }

    /// Re-scan the directory, picking up added, changed, or removed skills.
    pub fn reload(&mut self) -> &mut Self {
        self.scan();
        self
    }

    /// How many skills were discovered.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether no skills were discovered.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// The discovered skills, in name order.
    pub fn skills(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    /// Look up a single skill.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    fn scan(&mut self) {
        self.skills.clear();
        let Ok(entries) = fs::read_dir(&self.skills_dir) else {
            return;
        };
        let mut manifests: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path().join(SKILL_MANIFEST))
            .filter(|path| path.is_file())
            .collect();
        manifests.sort();
        for path in manifests {
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let (metadata, body) = parse_frontmatter(&content);
            let dir_name = path
                .parent()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let name = match metadata.get("name").map(String::as_str).map(str::trim) {
                Some(name) if !name.is_empty() => name.to_string(),
                _ => dir_name,
            };
            let fallback = body.lines().next().unwrap_or_default();
            let description = match metadata
                .get("description")
                .map(String::as_str)
                .map(str::trim)
            {
                Some(description) if !description.is_empty() => collapse(description),
                _ => collapse(fallback.trim_start_matches('#').trim()),
            };
            self.skills.insert(
                name.clone(),
                Skill {
                    name,
                    description,
                    content,
                    path,
                },
            );
        }
    }

    /// The catalog block: one `- name: description` line per skill.
    pub fn catalog(&self) -> String {
        if self.skills.is_empty() {
            return "(no skills found)".to_string();
        }
        self.skills
            .values()
            .map(|skill| format!("- {}: {}", skill.name, skill.description))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The block appended to a system prompt so the model knows what exists.
    ///
    /// The directory is named on purpose: "install this skill for me" is a
    /// request the model can only carry out if it knows where skills go.
    pub fn prompt_section(&self) -> String {
        format!(
            "Skills available:\n{}\n\nUse load_skill to read the full instructions when a \
             skill applies. Skills are folders holding a {SKILL_MANIFEST}, kept in {}; \
             that is where a new one goes.",
            self.catalog(),
            self.skills_dir.display()
        )
    }

    /// The full manifest for `name`, or [`Error::UnknownSkill`] listing what is
    /// available.
    pub fn load(&self, name: &str) -> Result<&str> {
        match self.skills.get(name) {
            Some(skill) => Ok(&skill.content),
            None => Err(Error::UnknownSkill {
                name: name.to_string(),
                available: {
                    if self.skills.is_empty() {
                        "none".to_string()
                    } else {
                        self.skills.keys().cloned().collect::<Vec<_>>().join(", ")
                    }
                },
            }),
        }
    }
}

/// Split a manifest into frontmatter metadata and the body that follows.
///
/// Only the small YAML subset the reference harness uses is supported: `key:
/// value` pairs (optionally quoted) and block scalars (`|` / `>`, with either
/// chomping indicator). A manifest with no leading `---` block is all body.
pub fn parse_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let lines: Vec<&str> = text.lines().collect();
    if lines.first().map(|line| line.trim_end()) != Some("---") {
        return (BTreeMap::new(), text.to_string());
    }
    let Some(closing) = lines
        .iter()
        .skip(1)
        .position(|line| line.trim_end() == "---")
        .map(|index| index + 1)
    else {
        return (BTreeMap::new(), text.to_string());
    };

    let mut metadata = BTreeMap::new();
    let mut index = 1;
    while index < closing {
        let line = lines[index];
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            index += 1;
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            index += 1;
            continue;
        };
        let key = key.trim().to_string();
        let value = value.trim();
        if key.is_empty() {
            index += 1;
            continue;
        }
        if is_block_scalar(value) {
            let folded = value.starts_with('>');
            let mut block: Vec<String> = Vec::new();
            index += 1;
            while index < closing {
                let candidate = lines[index];
                if !candidate.trim().is_empty() && !candidate.starts_with([' ', '\t']) {
                    break;
                }
                block.push(candidate.trim().to_string());
                index += 1;
            }
            let joined = if folded {
                block.join(" ")
            } else {
                block.join("\n")
            };
            metadata.insert(key, joined.trim().to_string());
            continue;
        }
        metadata.insert(key, unquote(value).to_string());
        index += 1;
    }

    let body = lines[closing + 1..].join("\n").trim().to_string();
    (metadata, body)
}

fn is_block_scalar(value: &str) -> bool {
    matches!(value, "|" | "|-" | "|+" | ">" | ">-" | ">+")
}

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

/// Collapse every run of whitespace into a single space.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, dir: &str, body: &str) -> PathBuf {
        let dir = root.join(dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SKILL_MANIFEST);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn frontmatter_is_parsed_and_stripped() {
        let text = "---\nname: demo\ndescription: A demo.\n---\n\n# Demo\n\nBody.\n";
        let (metadata, body) = parse_frontmatter(text);
        assert_eq!(metadata.get("name").map(String::as_str), Some("demo"));
        assert_eq!(
            metadata.get("description").map(String::as_str),
            Some("A demo.")
        );
        assert_eq!(body, "# Demo\n\nBody.");
    }

    #[test]
    fn frontmatter_absent_is_all_body() {
        let (metadata, body) = parse_frontmatter("# Just a body\n");
        assert!(metadata.is_empty());
        assert_eq!(body, "# Just a body\n");
    }

    #[test]
    fn block_scalar_descriptions_are_parsed() {
        let literal = "---\nname: x\ndescription: |\n  Line one\n  line two\n---\nbody";
        let (metadata, _) = parse_frontmatter(literal);
        assert_eq!(
            metadata.get("description").map(String::as_str),
            Some("Line one\nline two")
        );
        assert_eq!(
            collapse(metadata.get("description").unwrap()),
            "Line one line two"
        );

        let folded = "---\ndescription: >-\n  Line one\n  line two\n---\nbody";
        let (metadata, _) = parse_frontmatter(folded);
        assert_eq!(
            metadata.get("description").map(String::as_str),
            Some("Line one line two")
        );
    }

    #[test]
    fn quoted_description_is_unquoted() {
        let text = "---\ndescription: \"hello: world\"\n---\nx";
        let (metadata, _) = parse_frontmatter(text);
        assert_eq!(
            metadata.get("description").map(String::as_str),
            Some("hello: world")
        );
    }

    #[test]
    fn loader_scans_and_catalogs_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "beta",
            "Beta helper for things.\nMore detail.\n",
        );
        write_skill(
            dir.path(),
            "alpha",
            "---\nname: alpha\ndescription: Alpha skill.\n---\nbody",
        );

        let loader = SkillLoader::new(dir.path());
        assert_eq!(loader.len(), 2);
        assert_eq!(
            loader.catalog(),
            "- alpha: Alpha skill.\n- beta: Beta helper for things."
        );
        assert!(loader.load("alpha").unwrap().contains("Alpha skill"));
        assert!(loader.load("beta").unwrap().contains("More detail."));
        assert!(loader.prompt_section().contains("load_skill"));
        // The model is told where skills live, so it can add one itself.
        assert!(loader
            .prompt_section()
            .contains(&dir.path().display().to_string()));
    }

    #[test]
    fn unknown_skill_lists_available_names() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "alpha", "Alpha.\n");
        let loader = SkillLoader::new(dir.path());
        let error = loader.load("nope").unwrap_err().to_string();
        assert!(error.contains("unknown skill"), "{error}");
        assert!(error.contains("alpha"), "{error}");
    }

    #[test]
    fn missing_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let loader = SkillLoader::new(dir.path().join("does-not-exist"));
        assert!(loader.is_empty());
        assert_eq!(loader.catalog(), "(no skills found)");
    }
}
