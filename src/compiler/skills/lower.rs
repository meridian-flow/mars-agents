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

fn user_invocation_disabled(profile: &SkillProfile) -> bool {
    let _was_explicitly_set = profile.had_user_invocable_field;
    !profile.user_invocable
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
    if user_invocation_disabled(profile) {
        yaml.insert(yk("user-invocable"), Value::Bool(false));
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
    if user_invocation_disabled(profile) {
        lossy_fields.push(dropped("user-invocable", SkillHarness::Codex));
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
    if user_invocation_disabled(profile) {
        lossy_fields.push(dropped("user-invocable", SkillHarness::OpenCode));
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
    let mut lossy_fields = Vec::new();
    if user_invocation_disabled(profile) {
        lossy_fields.push(dropped("user-invocable", SkillHarness::Pi));
    }
    LoweredOutput {
        bytes: render(yaml, body),
        lossy_fields,
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
    if user_invocation_disabled(profile) {
        lossy_fields.push(dropped("user-invocable", SkillHarness::Cursor));
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

    fn parse_profile(content: &str) -> SkillProfile {
        let mut diags = Vec::new();
        parse_skill_content(content, &mut diags).unwrap().0
    }

    fn profile() -> SkillProfile {
        parse_profile(
            "---\nname: skill\ndescription: desc\nmodel-invocable: false\nallowed-tools: [Bash(git *)]\nlicense: MIT\nmetadata:\n  owner: team\nextra: stripped\n---\nBody\n",
        )
    }

    fn identity_profile() -> SkillProfile {
        parse_profile("---\nname: skill\ndescription: desc\n---\nBody\n")
    }

    fn user_invocable_false_profile() -> SkillProfile {
        parse_profile("---\nname: skill\ndescription: desc\nuser-invocable: false\n---\nBody\n")
    }

    fn explicit_true_profile() -> SkillProfile {
        parse_profile(
            "---\nname: skill\ndescription: desc\nmodel-invocable: true\nuser-invocable: true\n---\nBody\n",
        )
    }

    fn both_false_profile() -> SkillProfile {
        parse_profile(
            "---\nname: skill\ndescription: desc\nmodel-invocable: false\nuser-invocable: false\n---\nBody\n",
        )
    }

    fn has_dropped(lossy_fields: &[LossyField], field: &str, target: &str) -> bool {
        lossy_fields.iter().any(|f| {
            f.field == field && f.target == target && f.classification == Lossiness::Dropped
        })
    }

    #[test]
    fn claude_maps_model_invocation_and_tools() {
        let lowered = lower_skill_to_claude(&profile(), "Body\n");
        let out = String::from_utf8(lowered.bytes).unwrap();
        assert!(out.contains("disable-model-invocation: true"));
        assert!(out.contains("allowed-tools:"));
        assert!(!out.contains("allow_implicit_invocation"));
        assert!(out.contains("license: MIT"));
        assert!(out.contains("owner: team"));
        assert!(!out.contains("extra:"));
        assert!(lowered.lossy_fields.is_empty());
    }

    #[test]
    fn claude_emits_user_invocable_false() {
        let lowered = lower_skill_to_claude(&user_invocable_false_profile(), "Body\n");
        let out = String::from_utf8(lowered.bytes).unwrap();
        assert!(out.contains("user-invocable: false"));
        assert!(lowered.lossy_fields.is_empty());
    }

    #[test]
    fn claude_omits_user_invocable_when_true() {
        let out =
            String::from_utf8(lower_skill_to_claude(&explicit_true_profile(), "Body\n").bytes)
                .unwrap();
        assert!(!out.contains("user-invocable"));
    }

    #[test]
    fn claude_omits_disable_model_invocation_when_true() {
        let out =
            String::from_utf8(lower_skill_to_claude(&explicit_true_profile(), "Body\n").bytes)
                .unwrap();
        assert!(!out.contains("disable-model-invocation"));
        assert!(!out.contains("user-invocable"));
        assert!(!out.contains("allow_implicit_invocation"));
    }

    #[test]
    fn claude_both_false() {
        let lowered = lower_skill_to_claude(&both_false_profile(), "Body\n");
        let out = String::from_utf8(lowered.bytes).unwrap();
        assert!(out.contains("disable-model-invocation: true"));
        assert!(out.contains("user-invocable: false"));
        assert!(lowered.lossy_fields.is_empty());
    }

    #[test]
    fn codex_maps_model_invocation_and_drops_tools() {
        let lowered = lower_skill_to_codex(&profile(), "Body\n");
        let out = String::from_utf8(lowered.bytes).unwrap();
        assert!(out.contains("allow_implicit_invocation: false"));
        assert!(!out.contains("disable-model-invocation"));
        assert!(!out.contains("allowed-tools"));
        assert!(has_dropped(&lowered.lossy_fields, "allowed-tools", "Codex"));
    }

    #[test]
    fn codex_identity_only_does_not_gain_invocation_field() {
        let out =
            String::from_utf8(lower_skill_to_codex(&identity_profile(), "Body\n").bytes).unwrap();
        assert!(out.contains("name: skill"));
        assert!(out.contains("description: desc"));
        assert!(!out.contains("allow_implicit_invocation"));
    }

    #[test]
    fn codex_explicit_true_emits_allow_implicit_invocation_true() {
        let lowered = lower_skill_to_codex(&explicit_true_profile(), "Body\n");
        let out = String::from_utf8(lowered.bytes).unwrap();
        assert!(out.contains("allow_implicit_invocation: true"));
        assert!(!out.contains("disable-model-invocation"));
        assert!(!has_dropped(
            &lowered.lossy_fields,
            "user-invocable",
            "Codex"
        ));
    }

    #[test]
    fn codex_drops_user_invocable_false() {
        let lowered = lower_skill_to_codex(&user_invocable_false_profile(), "Body\n");
        assert!(has_dropped(
            &lowered.lossy_fields,
            "user-invocable",
            "Codex"
        ));
    }

    #[test]
    fn codex_no_lossiness_user_invocable_true() {
        let lowered = lower_skill_to_codex(&identity_profile(), "Body\n");
        assert!(!has_dropped(
            &lowered.lossy_fields,
            "user-invocable",
            "Codex"
        ));
    }

    #[test]
    fn opencode_drops_model_invocable_and_tools() {
        let lowered = lower_skill_to_opencode(&profile(), "Body\n");
        assert!(has_dropped(
            &lowered.lossy_fields,
            "model-invocable",
            "OpenCode"
        ));
        assert!(has_dropped(
            &lowered.lossy_fields,
            "allowed-tools",
            "OpenCode"
        ));
        assert_eq!(lowered.lossy_fields.len(), 2);
    }

    #[test]
    fn opencode_drops_user_invocable_false() {
        let lowered = lower_skill_to_opencode(&user_invocable_false_profile(), "Body\n");
        assert!(has_dropped(
            &lowered.lossy_fields,
            "user-invocable",
            "OpenCode"
        ));
    }

    #[test]
    fn opencode_no_invocability_lossiness_when_defaults() {
        let lowered = lower_skill_to_opencode(&identity_profile(), "Body\n");
        assert!(!has_dropped(
            &lowered.lossy_fields,
            "model-invocable",
            "OpenCode"
        ));
        assert!(!has_dropped(
            &lowered.lossy_fields,
            "user-invocable",
            "OpenCode"
        ));
        assert!(lowered.lossy_fields.is_empty());
    }

    #[test]
    fn pi_model_false_emits_disable_model_invocation() {
        let lowered = lower_skill_to_pi(&profile(), "Body\n");
        let out = String::from_utf8(lowered.bytes).unwrap();
        assert!(out.contains("disable-model-invocation: true"));
    }

    #[test]
    fn pi_drops_user_invocable_false() {
        let lowered = lower_skill_to_pi(&user_invocable_false_profile(), "Body\n");
        let out = String::from_utf8(lowered.bytes).unwrap();
        assert!(!out.contains("user-invocable"));
        assert!(has_dropped(&lowered.lossy_fields, "user-invocable", "Pi"));
    }

    #[test]
    fn pi_model_true_omits_disable_model_invocation_and_user_true_no_lossiness() {
        let lowered = lower_skill_to_pi(&explicit_true_profile(), "Body\n");
        let out = String::from_utf8(lowered.bytes).unwrap();
        assert!(!out.contains("disable-model-invocation"));
        assert!(!out.contains("user-invocable"));
        assert!(!has_dropped(&lowered.lossy_fields, "user-invocable", "Pi"));
    }

    #[test]
    fn cursor_drops_tools() {
        let lowered = lower_skill_to_cursor(&profile(), "Body\n");
        let out = String::from_utf8(lowered.bytes).unwrap();
        assert!(out.contains("disable-model-invocation: true"));
        assert!(!out.contains("allowed-tools"));
        assert_eq!(lowered.lossy_fields.len(), 1);
    }

    #[test]
    fn cursor_drops_user_invocable_false() {
        let lowered = lower_skill_to_cursor(&user_invocable_false_profile(), "Body\n");
        let out = String::from_utf8(lowered.bytes).unwrap();
        assert!(!out.contains("user-invocable"));
        assert!(has_dropped(
            &lowered.lossy_fields,
            "user-invocable",
            "Cursor"
        ));
    }

    #[test]
    fn cursor_model_true_omits_disable_model_invocation_and_user_true_no_lossiness() {
        let lowered = lower_skill_to_cursor(&explicit_true_profile(), "Body\n");
        let out = String::from_utf8(lowered.bytes).unwrap();
        assert!(!out.contains("disable-model-invocation"));
        assert!(!out.contains("user-invocable"));
        assert!(!has_dropped(
            &lowered.lossy_fields,
            "user-invocable",
            "Cursor"
        ));
    }

    #[test]
    fn no_frontmatter_body_only_all_harnesses() {
        let mut diags = Vec::new();
        let (profile, fm) = parse_skill_content("# Body\nbytes", &mut diags).unwrap();
        let body = fm.body();
        for harness in [
            SkillHarness::Claude,
            SkillHarness::Codex,
            SkillHarness::OpenCode,
            SkillHarness::Pi,
            SkillHarness::Cursor,
        ] {
            let out =
                String::from_utf8(lower_skill_for_harness(harness, &profile, body).bytes).unwrap();
            assert_eq!(out, "# Body\nbytes");
        }
    }
}
