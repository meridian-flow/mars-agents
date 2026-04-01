use indexmap::{IndexMap, IndexSet};
use serde_yaml::{Mapping, Value};

/// Parsed markdown frontmatter and body.
#[derive(Debug, Clone)]
pub struct Frontmatter {
    yaml: Mapping,
    body: String,
    has_frontmatter: bool,
}

/// Errors from frontmatter parsing.
#[derive(Debug, thiserror::Error)]
pub enum FrontmatterError {
    #[error("malformed YAML frontmatter: {0}")]
    MalformedYaml(#[from] serde_yaml::Error),

    #[error("frontmatter is not a YAML mapping")]
    NotAMapping,
}

/// Parse markdown content into frontmatter and body.
pub fn parse(content: &str) -> Result<Frontmatter, FrontmatterError> {
    Frontmatter::parse(content)
}

impl Frontmatter {
    /// Parse a markdown document into frontmatter + body.
    pub fn parse(content: &str) -> Result<Self, FrontmatterError> {
        let (first_line, after_first_line) = split_first_line(content);
        if !is_delimiter_line(first_line) {
            return Ok(Self {
                yaml: Mapping::new(),
                body: content.to_string(),
                has_frontmatter: false,
            });
        }

        let mut yaml_end = None;
        let mut offset = 0usize;
        for line in after_first_line.split_inclusive('\n') {
            if is_delimiter_line(line) {
                yaml_end = Some((offset, line.len()));
                break;
            }
            offset += line.len();
        }

        let Some((yaml_len, closing_len)) = yaml_end else {
            return Ok(Self {
                yaml: Mapping::new(),
                body: content.to_string(),
                has_frontmatter: false,
            });
        };

        let yaml_text = &after_first_line[..yaml_len];
        let body_start = yaml_len + closing_len;
        let body = after_first_line[body_start..].to_string();

        if yaml_text.trim().is_empty() {
            return Ok(Self {
                yaml: Mapping::new(),
                body,
                has_frontmatter: true,
            });
        }

        let value: Value = serde_yaml::from_str(yaml_text)?;
        let yaml = match value {
            Value::Mapping(mapping) => mapping,
            Value::Null => Mapping::new(),
            _ => return Err(FrontmatterError::NotAMapping),
        };

        Ok(Self {
            yaml,
            body,
            has_frontmatter: true,
        })
    }

    /// Read the `skills` list.
    pub fn skills(&self) -> Vec<String> {
        self.get("skills")
            .and_then(Value::as_sequence)
            .map(|skills| {
                skills
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Replace the `skills` list.
    pub fn set_skills(&mut self, skills: Vec<String>) {
        let key = yaml_key("skills");
        if skills.is_empty() {
            self.yaml.remove(&key);
            return;
        }

        let sequence = skills.into_iter().map(Value::String).collect();
        self.yaml.insert(key, Value::Sequence(sequence));
    }

    /// Read the `name` field if present.
    pub fn name(&self) -> Option<&str> {
        self.get("name").and_then(Value::as_str)
    }

    /// Read any YAML field by key.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.yaml.get(yaml_key(key))
    }

    /// Markdown body after frontmatter.
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Whether this document contains frontmatter delimiters.
    pub fn has_frontmatter(&self) -> bool {
        self.has_frontmatter
    }

    /// Serialize back to full markdown.
    pub fn render(&self) -> String {
        if !self.has_frontmatter && self.yaml.is_empty() {
            return self.body.clone();
        }

        let mut out = String::from("---\n");
        if !self.yaml.is_empty() {
            let mut yaml = serde_yaml::to_string(&self.yaml)
                .expect("serializing frontmatter mapping should succeed");
            if let Some(stripped) = yaml.strip_prefix("---\n") {
                yaml = stripped.to_string();
            }
            out.push_str(&yaml);
            if !yaml.ends_with('\n') {
                out.push('\n');
            }
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        out
    }
}

/// Rename skills in frontmatter using exact-match replacement.
pub fn rewrite_skills(
    fm: &mut Frontmatter,
    renames: &IndexMap<String, String>,
) -> IndexSet<String> {
    let mut renamed = IndexSet::new();
    let mut skills = fm.skills();

    for skill in &mut skills {
        if let Some(new_name) = renames.get(skill.as_str()) {
            renamed.insert(skill.clone());
            *skill = new_name.clone();
        }
    }

    if !renamed.is_empty() {
        fm.set_skills(skills);
    }

    renamed
}

/// Parse content, rewrite skills, and render updated content if changed.
pub fn rewrite_content_skills(
    content: &str,
    renames: &IndexMap<String, String>,
) -> Result<Option<String>, FrontmatterError> {
    let mut fm = Frontmatter::parse(content)?;
    let renamed = rewrite_skills(&mut fm, renames);
    if renamed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(fm.render()))
    }
}

fn split_first_line(content: &str) -> (&str, &str) {
    match content.split_once('\n') {
        Some((first, rest)) => (first, rest),
        None => (content, ""),
    }
}

fn is_delimiter_line(line: &str) -> bool {
    line.trim_end() == "---"
}

fn yaml_key(key: &str) -> Value {
    Value::String(key.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_render_roundtrip() {
        let input = "---\nname: coder\nskills:\n- plan\n- review\n---\n# Body\ntext";
        let fm = Frontmatter::parse(input).unwrap();
        assert_eq!(fm.name(), Some("coder"));
        assert_eq!(fm.skills(), vec!["plan", "review"]);
        assert_eq!(fm.body(), "# Body\ntext");
        assert!(fm.has_frontmatter());

        let rendered = fm.render();
        let reparsed = Frontmatter::parse(&rendered).unwrap();
        assert_eq!(reparsed.name(), Some("coder"));
        assert_eq!(reparsed.skills(), vec!["plan", "review"]);
        assert_eq!(reparsed.body(), "# Body\ntext");
    }

    #[test]
    fn parse_without_frontmatter_keeps_body() {
        let input = "# Markdown only\ntext";
        let fm = parse(input).unwrap();
        assert!(!fm.has_frontmatter());
        assert!(fm.skills().is_empty());
        assert_eq!(fm.body(), input);
        assert_eq!(fm.render(), input);
    }

    #[test]
    fn parse_empty_frontmatter_roundtrips_delimiters() {
        let input = "---\n---\nbody";
        let fm = Frontmatter::parse(input).unwrap();
        assert!(fm.has_frontmatter());
        assert!(fm.skills().is_empty());
        assert_eq!(fm.body(), "body");
        assert_eq!(fm.render(), input);
    }

    #[test]
    fn parse_malformed_yaml_errors() {
        let input = "---\ninvalid: [:\n---\nbody";
        assert!(matches!(
            Frontmatter::parse(input),
            Err(FrontmatterError::MalformedYaml(_))
        ));
    }

    #[test]
    fn parse_flow_style_skills() {
        let input = "---\nskills: [plan, review]\n---\nbody";
        let fm = Frontmatter::parse(input).unwrap();
        assert_eq!(fm.skills(), vec!["plan", "review"]);
    }

    #[test]
    fn rewrite_does_not_corrupt_substrings() {
        let input = "---\nskills:\n- plan\n- planner\n- planning-extended\n---\nbody\n";
        let renames =
            IndexMap::from([("plan".to_string(), "plan__haowjy_meridian-base".to_string())]);

        let rewritten = rewrite_content_skills(input, &renames).unwrap().unwrap();
        let fm = Frontmatter::parse(&rewritten).unwrap();
        assert_eq!(
            fm.skills(),
            vec!["plan__haowjy_meridian-base", "planner", "planning-extended"]
        );
    }
}
