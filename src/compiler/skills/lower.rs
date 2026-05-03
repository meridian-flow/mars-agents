//! Per-harness lowering for universal skill frontmatter.

use serde_yaml::{Mapping, Value};

use crate::compiler::agents::lower::{Lossiness, LossyField, LoweredOutput};
use crate::compiler::skills::SkillProfile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillHarness {
    Claude,
    Codex,
    OpenCode,
    Pi,
    Cursor,
}

impl SkillHarness {
    pub fn from_variant_key(key: &str) -> Option<Self> {
        match key {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::OpenCode),
            "pi" => Some(Self::Pi),
            "cursor" => Some(Self::Cursor),
            _ => None,
        }
    }
    fn target_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
            Self::OpenCode => "OpenCode",
            Self::Pi => "Pi",
            Self::Cursor => "Cursor",
        }
    }
}

fn yk(s: &str) -> Value {
    Value::String(s.to_string())
}
fn ys(s: &str) -> Value {
    Value::String(s.to_string())
}
fn insert_identity(yaml: &mut Mapping, profile: &SkillProfile) {
    if let Some(name) = &profile.name {
        yaml.insert(yk("name"), ys(name));
    }
    if let Some(description) = &profile.description {
        yaml.insert(yk("description"), ys(description));
    }
}
fn insert_allowed_tools(yaml: &mut Mapping, profile: &SkillProfile) {
    if !profile.allowed_tools.is_empty() {
        yaml.insert(
            yk("allowed-tools"),
            Value::Sequence(profile.allowed_tools.iter().map(|s| ys(s)).collect()),
        );
    }
}
fn insert_metadata(yaml: &mut Mapping, profile: &SkillProfile) {
    if let Some(license) = &profile.license {
        yaml.insert(yk("license"), ys(license));
    }
    if let Some(metadata) = &profile.metadata {
        yaml.insert(yk("metadata"), metadata.clone());
    }
}

fn render(yaml: Mapping, body: &str) -> Vec<u8> {
    if yaml.is_empty() {
        return body.as_bytes().to_vec();
    }
    let mut yaml_str = serde_yaml::to_string(&yaml).expect("skill frontmatter should serialize");
    if let Some(stripped) = yaml_str.strip_prefix("---\n") {
        yaml_str = stripped.to_string();
    }
    let mut out = String::from("---\n");
    out.push_str(&yaml_str);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("---\n");
    out.push_str(body);
    out.into_bytes()
}

pub fn lower_skill_for_harness(
    harness: SkillHarness,
    profile: &SkillProfile,
    body: &str,
) -> LoweredOutput {
    match harness {
        SkillHarness::Claude => lower_skill_to_claude(profile, body),
        SkillHarness::Codex => lower_skill_to_codex(profile, body),
        SkillHarness::OpenCode => lower_skill_to_opencode(profile, body),
        SkillHarness::Pi => lower_skill_to_pi(profile, body),
        SkillHarness::Cursor => lower_skill_to_cursor(profile, body),
    }
}

pub fn lower_skill_to_claude(profile: &SkillProfile, body: &str) -> LoweredOutput {
    let mut yaml = Mapping::new();
    insert_identity(&mut yaml, profile);
    if !profile.model_invocable {
        yaml.insert(yk("disable-model-invocation"), Value::Bool(true));
    }
    insert_allowed_tools(&mut yaml, profile);
    insert_metadata(&mut yaml, profile);
    LoweredOutput {
        bytes: render(yaml, body),
        lossy_fields: vec![],
    }
}

pub fn lower_skill_to_codex(profile: &SkillProfile, body: &str) -> LoweredOutput {
    let mut yaml = Mapping::new();
    insert_identity(&mut yaml, profile);
    if profile.had_model_invocable_field {
        yaml.insert(
            yk("allow_implicit_invocation"),
            Value::Bool(profile.model_invocable),
        );
    }
    insert_metadata(&mut yaml, profile);
    let mut lossy_fields = Vec::new();
    if !profile.allowed_tools.is_empty() {
        lossy_fields.push(dropped("allowed-tools", SkillHarness::Codex));
    }
    LoweredOutput {
        bytes: render(yaml, body),
        lossy_fields,
    }
}

pub fn lower_skill_to_opencode(profile: &SkillProfile, body: &str) -> LoweredOutput {
    let mut yaml = Mapping::new();
    insert_identity(&mut yaml, profile);
    insert_metadata(&mut yaml, profile);
    let mut lossy_fields = Vec::new();
    if !profile.model_invocable {
        lossy_fields.push(dropped("model-invocable", SkillHarness::OpenCode));
    }
    if !profile.allowed_tools.is_empty() {
        lossy_fields.push(dropped("allowed-tools", SkillHarness::OpenCode));
    }
    LoweredOutput {
        bytes: render(yaml, body),
        lossy_fields,
    }
}

pub fn lower_skill_to_pi(profile: &SkillProfile, body: &str) -> LoweredOutput {
    let mut yaml = Mapping::new();
    insert_identity(&mut yaml, profile);
    if !profile.model_invocable {
        yaml.insert(yk("disable-model-invocation"), Value::Bool(true));
    }
    insert_allowed_tools(&mut yaml, profile);
    insert_metadata(&mut yaml, profile);
    LoweredOutput {
        bytes: render(yaml, body),
        lossy_fields: vec![],
    }
}

pub fn lower_skill_to_cursor(profile: &SkillProfile, body: &str) -> LoweredOutput {
    let mut yaml = Mapping::new();
    insert_identity(&mut yaml, profile);
    if !profile.model_invocable {
        yaml.insert(yk("disable-model-invocation"), Value::Bool(true));
    }
    insert_metadata(&mut yaml, profile);
    let mut lossy_fields = Vec::new();
    if !profile.allowed_tools.is_empty() {
        lossy_fields.push(dropped("allowed-tools", SkillHarness::Cursor));
    }
    LoweredOutput {
        bytes: render(yaml, body),
        lossy_fields,
    }
}

fn dropped(field: &str, harness: SkillHarness) -> LossyField {
    LossyField {
        field: field.to_string(),
        target: harness.target_name().to_string(),
        classification: Lossiness::Dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::skills::parse_skill_content;
    fn profile() -> SkillProfile {
        let content = "---\nname: skill\ndescription: desc\nmodel-invocable: false\nallowed-tools: [Bash(git *)]\nlicense: MIT\nmetadata:\n  owner: team\nextra: stripped\n---\nBody\n";
        let mut diags = Vec::new();
        parse_skill_content(content, &mut diags).unwrap().0
    }
    #[test]
    fn claude_maps_invocation_and_tools() {
        let out = String::from_utf8(lower_skill_to_claude(&profile(), "Body\n").bytes).unwrap();
        assert!(out.contains("disable-model-invocation: true"));
        assert!(out.contains("allowed-tools:"));
        assert!(!out.contains("allow_implicit_invocation"));
        assert!(!out.contains("extra:"));
    }
    #[test]
    fn codex_maps_invocation_and_drops_tools() {
        let lowered = lower_skill_to_codex(&profile(), "Body\n");
        let out = String::from_utf8(lowered.bytes).unwrap();
        assert!(out.contains("allow_implicit_invocation: false"));
        assert!(!out.contains("disable-model-invocation"));
        assert!(!out.contains("allowed-tools"));
        assert!(
            lowered
                .lossy_fields
                .iter()
                .any(|f| f.field == "allowed-tools" && f.classification == Lossiness::Dropped)
        );
    }
    #[test]
    fn codex_identity_only_does_not_gain_invocation_field() {
        let content = "---\nname: skill\ndescription: desc\n---\nBody\n";
        let mut diags = Vec::new();
        let (profile, _) = parse_skill_content(content, &mut diags).unwrap();
        let out = String::from_utf8(lower_skill_to_codex(&profile, "Body\n").bytes).unwrap();
        assert!(out.contains("name: skill"));
        assert!(out.contains("description: desc"));
        assert!(!out.contains("allow_implicit_invocation"));
    }

    #[test]
    fn codex_no_frontmatter_copies_body_without_frontmatter() {
        let mut diags = Vec::new();
        let (profile, fm) = parse_skill_content("# Body\nbytes", &mut diags).unwrap();
        let out = String::from_utf8(lower_skill_to_codex(&profile, fm.body()).bytes).unwrap();
        assert_eq!(out, "# Body\nbytes");
    }

    #[test]
    fn opencode_drops_invocation_and_tools() {
        let lowered = lower_skill_to_opencode(&profile(), "Body\n");
        assert_eq!(lowered.lossy_fields.len(), 2);
    }
    #[test]
    fn cursor_drops_tools() {
        let lowered = lower_skill_to_cursor(&profile(), "Body\n");
        let out = String::from_utf8(lowered.bytes).unwrap();
        assert!(out.contains("disable-model-invocation: true"));
        assert!(!out.contains("allowed-tools"));
        assert_eq!(lowered.lossy_fields.len(), 1);
    }
}
