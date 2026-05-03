//! Universal skill frontmatter parser and native lowering support.

pub mod lower;

use serde_yaml::Value;

use crate::frontmatter::{Frontmatter, FrontmatterError};

#[derive(Debug, Clone)]
pub struct SkillProfile {
    pub name: Option<String>,
    pub description: Option<String>,
    pub model_invocable: bool,
    #[allow(dead_code)]
    pub user_invocable: bool,
    pub allowed_tools: Vec<String>,
    pub license: Option<String>,
    pub metadata: Option<Value>,
    /// true when the source frontmatter explicitly set `model-invocable`
    pub had_model_invocable_field: bool,
    /// true when the source frontmatter explicitly set `user-invocable`
    #[allow(dead_code)]
    pub had_user_invocable_field: bool,
    pub has_frontmatter: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillDiagnostic {
    InvalidFieldValue {
        field: String,
        value: String,
        allowed: &'static str,
    },
    InvalidFieldType {
        field: String,
        value: String,
        allowed: &'static str,
    },
    RemovedField {
        field: String,
    },
    MalformedFrontmatter {
        message: String,
    },
}

impl SkillDiagnostic {
    pub fn is_error(&self) -> bool {
        matches!(
            self,
            Self::InvalidFieldValue { .. }
                | Self::RemovedField { .. }
                | Self::MalformedFrontmatter { .. }
        )
    }

    pub fn message(&self) -> String {
        match self {
            Self::InvalidFieldValue {
                field,
                value,
                allowed,
            } => format!("skill field `{field}` has invalid value `{value}`; allowed: {allowed}"),
            Self::InvalidFieldType {
                field,
                value,
                allowed,
            } => format!(
                "skill field `{field}` has unsupported value `{value}`; expected: {allowed}"
            ),
            Self::RemovedField { field } => format!(
                "skill field `{field}` has been removed; use `model-invocable` / `user-invocable` instead"
            ),
            Self::MalformedFrontmatter { message } => {
                format!("skill frontmatter is malformed; raw fallback used: {message}")
            }
        }
    }
}

fn value_label(val: &Value) -> String {
    val.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{val:?}"))
}

fn yaml_str_list(field: &str, val: &Value, diags: &mut Vec<SkillDiagnostic>) -> Vec<String> {
    match val {
        Value::Sequence(seq) => seq
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| match item.as_str() {
                Some(s) => Some(s.to_owned()),
                None => {
                    diags.push(SkillDiagnostic::InvalidFieldType {
                        field: format!("{field}[{idx}]"),
                        value: value_label(item),
                        allowed: "string",
                    });
                    None
                }
            })
            .collect(),
        Value::String(s) => vec![s.clone()],
        _ => {
            diags.push(SkillDiagnostic::InvalidFieldType {
                field: field.to_string(),
                value: value_label(val),
                allowed: "string or list of strings",
            });
            vec![]
        }
    }
}

fn validate_required_string(field: &str, val: Option<&Value>, diags: &mut Vec<SkillDiagnostic>) {
    match val {
        Some(raw) if raw.is_string() => {}
        Some(raw) => diags.push(SkillDiagnostic::InvalidFieldValue {
            field: field.to_string(),
            value: value_label(raw),
            allowed: "string",
        }),
        None => diags.push(SkillDiagnostic::InvalidFieldValue {
            field: field.to_string(),
            value: "missing".to_string(),
            allowed: "string",
        }),
    }
}

fn parse_invocability_bool(
    field: &str,
    raw: Option<&Value>,
    diags: &mut Vec<SkillDiagnostic>,
) -> (bool, bool) {
    match raw {
        Some(raw) => match raw.as_bool() {
            Some(value) => (value, true),
            None => {
                diags.push(SkillDiagnostic::InvalidFieldType {
                    field: field.to_string(),
                    value: value_label(raw),
                    allowed: "boolean",
                });
                (true, false)
            }
        },
        None => (true, false),
    }
}

pub fn parse_skill_profile(fm: &Frontmatter, diags: &mut Vec<SkillDiagnostic>) -> SkillProfile {
    let name_raw = fm.get("name");
    let name = name_raw.and_then(Value::as_str).map(str::to_owned);
    let description_raw = fm.get("description");
    let description = description_raw.and_then(Value::as_str).map(str::to_owned);
    if fm.has_frontmatter() {
        validate_required_string("name", name_raw, diags);
        validate_required_string("description", description_raw, diags);
    }
    let allowed_tools = fm
        .get("allowed-tools")
        .map(|v| yaml_str_list("allowed-tools", v, diags))
        .unwrap_or_default();
    let license_raw = fm.get("license");
    let license = license_raw.and_then(Value::as_str).map(str::to_owned);
    if let Some(raw) = license_raw
        && !raw.is_string()
    {
        diags.push(SkillDiagnostic::InvalidFieldType {
            field: "license".to_string(),
            value: value_label(raw),
            allowed: "string",
        });
    }
    let metadata = fm.get("metadata").cloned();

    let (model_invocable, had_model_invocable_field) =
        parse_invocability_bool("model-invocable", fm.get("model-invocable"), diags);
    let (user_invocable, had_user_invocable_field) =
        parse_invocability_bool("user-invocable", fm.get("user-invocable"), diags);

    for field in [
        "invocation",
        "disable-model-invocation",
        "allow_implicit_invocation",
    ] {
        if fm.get(field).is_some() {
            diags.push(SkillDiagnostic::RemovedField {
                field: field.to_string(),
            });
        }
    }

    SkillProfile {
        name,
        description,
        model_invocable,
        user_invocable,
        allowed_tools,
        license,
        metadata,
        had_model_invocable_field,
        had_user_invocable_field,
        has_frontmatter: fm.has_frontmatter(),
    }
}

pub fn parse_skill_content(
    content: &str,
    diags: &mut Vec<SkillDiagnostic>,
) -> Result<(SkillProfile, Frontmatter), FrontmatterError> {
    let fm = Frontmatter::parse(content).inspect_err(|e| {
        diags.push(SkillDiagnostic::MalformedFrontmatter {
            message: e.to_string(),
        });
    })?;
    let profile = parse_skill_profile(&fm, diags);
    Ok((profile, fm))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(content: &str) -> (SkillProfile, Vec<SkillDiagnostic>, Frontmatter) {
        let mut diags = Vec::new();
        let (profile, fm) = parse_skill_content(content, &mut diags).unwrap();
        (profile, diags, fm)
    }

    fn removed_field_named(diags: &[SkillDiagnostic], expected: &str) -> bool {
        diags.iter().any(|d| {
            matches!(
                d,
                SkillDiagnostic::RemovedField { field } if field == expected
            )
        })
    }

    #[test]
    fn no_frontmatter_defaults_invocable_and_preserves_body() {
        let (p, d, fm) = parse("# Body\nbytes");
        assert!(d.is_empty());
        assert!(p.model_invocable);
        assert!(p.user_invocable);
        assert_eq!(fm.body(), "# Body\nbytes");
    }

    #[test]
    fn parses_identity_only() {
        let (p, d, _) = parse("---\nname: a\ndescription: b\n---\nbody");
        assert!(d.is_empty());
        assert_eq!(p.name.as_deref(), Some("a"));
        assert_eq!(p.description.as_deref(), Some("b"));
    }

    #[test]
    fn model_invocable_false_parses() {
        let (p, d, _) = parse("---\nname: a\ndescription: b\nmodel-invocable: false\n---\nbody");
        assert!(d.is_empty());
        assert!(!p.model_invocable);
        assert!(p.had_model_invocable_field);
        assert!(p.user_invocable);
        assert!(!p.had_user_invocable_field);
    }

    #[test]
    fn user_invocable_false_parses() {
        let (p, d, _) = parse("---\nname: a\ndescription: b\nuser-invocable: false\n---\nbody");
        assert!(d.is_empty());
        assert!(p.model_invocable);
        assert!(!p.had_model_invocable_field);
        assert!(!p.user_invocable);
        assert!(p.had_user_invocable_field);
    }

    #[test]
    fn both_booleans_false_accepted() {
        let (p, d, _) = parse(
            "---\nname: a\ndescription: b\nmodel-invocable: false\nuser-invocable: false\n---\nbody",
        );
        assert!(d.is_empty());
        assert!(!p.model_invocable);
        assert!(!p.user_invocable);
        assert!(p.had_model_invocable_field);
        assert!(p.had_user_invocable_field);
    }

    #[test]
    fn non_boolean_model_invocable_defaults_true() {
        let (p, d, _) = parse("---\nname: a\ndescription: b\nmodel-invocable: \"yes\"\n---\nbody");
        assert!(p.model_invocable);
        assert!(!p.had_model_invocable_field);
        assert!(d.iter().any(|d| matches!(
            d,
            SkillDiagnostic::InvalidFieldType { field, allowed, .. }
                if field == "model-invocable" && *allowed == "boolean"
        )));
    }

    #[test]
    fn removed_field_invocation() {
        let (p, d, _) = parse("---\nname: a\ndescription: b\ninvocation: explicit\n---\nbody");
        assert!(p.model_invocable);
        assert!(p.user_invocable);
        assert!(removed_field_named(&d, "invocation"));
        assert!(d.iter().any(SkillDiagnostic::is_error));
    }

    #[test]
    fn removed_field_disable_model_invocation() {
        let (_, d, _) =
            parse("---\nname: a\ndescription: b\ndisable-model-invocation: true\n---\nbody");
        assert!(removed_field_named(&d, "disable-model-invocation"));
        assert!(d.iter().any(SkillDiagnostic::is_error));
    }

    #[test]
    fn removed_field_allow_implicit_invocation() {
        let (_, d, _) =
            parse("---\nname: a\ndescription: b\nallow_implicit_invocation: false\n---\nbody");
        assert!(removed_field_named(&d, "allow_implicit_invocation"));
        assert!(d.iter().any(SkillDiagnostic::is_error));
    }

    #[test]
    fn all_removed_fields_emit_removed_field() {
        let (_, d, _) = parse(
            "---\nname: a\ndescription: b\ninvocation: explicit\ndisable-model-invocation: true\nallow_implicit_invocation: false\n---\nbody",
        );
        assert!(removed_field_named(&d, "invocation"));
        assert!(removed_field_named(&d, "disable-model-invocation"));
        assert!(removed_field_named(&d, "allow_implicit_invocation"));
    }

    #[test]
    fn frontmatter_requires_name_and_description() {
        let (_, d, _) = parse("---\nname: a\n---\nbody");
        assert!(d.iter().any(|d| matches!(
            d,
            SkillDiagnostic::InvalidFieldValue { field, value, .. }
                if field == "description" && value == "missing"
        )));
    }

    #[test]
    fn warns_for_filtered_non_string_fields() {
        let (_, d, _) = parse(
            "---\nname: a\ndescription: b\nallowed-tools: [Bash(git *), 7]\nlicense: false\n---\nbody",
        );
        assert!(d.iter().any(|d| matches!(
            d,
            SkillDiagnostic::InvalidFieldType { field, .. } if field == "allowed-tools[1]"
        )));
        assert!(d.iter().any(|d| matches!(
            d,
            SkillDiagnostic::InvalidFieldType { field, .. } if field == "license"
        )));
    }

    #[test]
    fn malformed_yaml_raw_fallback_diagnostic() {
        let mut diags = Vec::new();
        let err = parse_skill_content("---\ninvalid: [:\n---\nbody", &mut diags).unwrap_err();
        assert!(matches!(err, FrontmatterError::MalformedYaml(_)));
        assert!(matches!(
            diags[0],
            SkillDiagnostic::MalformedFrontmatter { .. }
        ));
    }
}
