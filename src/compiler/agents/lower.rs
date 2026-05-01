/// Per-target agent lowering — translates a parsed [`AgentProfile`] into
/// harness-native format bytes.
///
/// # Lossiness classification (per agent-compilation-mapping.md §6)
///
/// Every field lowering is classified as:
/// - **exact** — field maps 1:1 to a native equivalent with identical semantics
/// - **approximate** — semantic equivalent exists but gap is noted
/// - **dropped** — no native equivalent; value is discarded in native artifact
/// - **meridian-only** — consumed exclusively by Meridian; never lowered
///
/// Dropped fields with non-default values emit [`LossyField`] diagnostics.
use crate::compiler::agents::{AgentDiagnostic, AgentProfile, HarnessKind, OverrideFields};
use crate::frontmatter::Frontmatter;

// ---------------------------------------------------------------------------
// Lossiness result types
// ---------------------------------------------------------------------------

/// A field that was dropped or only approximately lowered in the native artifact.
#[derive(Debug, Clone)]
pub struct LossyField {
    pub field: String,
    pub target: String,
    pub classification: Lossiness,
}

/// Lossiness classification for a single field in a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lossiness {
    Exact,
    Approximate { note: &'static str },
    Dropped,
    MeridianOnly,
}

/// Output from a single lowering pass.
pub struct LoweredOutput {
    /// Serialized bytes for the native artifact.
    pub bytes: Vec<u8>,
    /// Lossiness findings for fields that were dropped or approximated.
    pub lossy_fields: Vec<LossyField>,
}

// ---------------------------------------------------------------------------
// Effective field resolution — applies harness-overrides before lowering
// ---------------------------------------------------------------------------

/// Effective field values after merging profile defaults + harness override.
struct Effective<'a> {
    profile: &'a AgentProfile,
    over: Option<&'a OverrideFields>,
}

impl<'a> Effective<'a> {
    fn new(profile: &'a AgentProfile, harness: &HarnessKind) -> Self {
        let over = profile.harness_overrides.get(harness);
        Self { profile, over }
    }

    fn effort(&self) -> Option<&crate::compiler::agents::EffortLevel> {
        self.over.and_then(|o| o.effort.as_ref()).or(self.profile.effort.as_ref())
    }

    fn approval(&self) -> Option<&crate::compiler::agents::ApprovalMode> {
        self.over.and_then(|o| o.approval.as_ref()).or(self.profile.approval.as_ref())
    }

    fn sandbox(&self) -> Option<&crate::compiler::agents::SandboxMode> {
        self.over.and_then(|o| o.sandbox.as_ref()).or(self.profile.sandbox.as_ref())
    }

    fn skills(&self) -> &[String] {
        if let Some(ov) = self.over.and_then(|o| o.skills.as_ref()) {
            return ov;
        }
        &self.profile.skills
    }

    fn tools(&self) -> &[String] {
        if let Some(ov) = self.over.and_then(|o| o.tools.as_ref()) {
            return ov;
        }
        &self.profile.tools
    }

    fn disallowed_tools(&self) -> &[String] {
        if let Some(ov) = self.over.and_then(|o| o.disallowed_tools.as_ref()) {
            return ov;
        }
        &self.profile.disallowed_tools
    }
}

// ---------------------------------------------------------------------------
// Meridian artifact — full-fidelity pass-through
// ---------------------------------------------------------------------------

/// Produce the Meridian-artifact bytes.
///
/// The Meridian artifact is full-fidelity: it is the source content verbatim,
/// with no field stripping. The `translated_content` in the translate stage is
/// `None` for the Meridian artifact (pass-through reads the source directly).
///
/// This function exists for testing and explicit production; the sync pipeline
/// uses the source content directly for the `.agents/` artifact.
pub fn lower_to_meridian(source_content: &str) -> LoweredOutput {
    LoweredOutput {
        bytes: source_content.as_bytes().to_vec(),
        lossy_fields: vec![],
    }
}

// ---------------------------------------------------------------------------
// Claude native artifact
// ---------------------------------------------------------------------------

/// Lower an agent profile to Claude-native markdown format.
///
/// Per agent-compilation-mapping.md V0 §10:
/// - Preserved: name, description, model, skills, tools, disallowed-tools, body
/// - Dropped (launch-time): approval, sandbox, mode, harness, autocompact,
///   model-policies, harness-overrides (claude entry merged before lowering),
///   fanout, legacy-models
///
/// `harness-overrides.claude` values are merged into top-level fields
/// before lowering (D42 — compile-time merge).
pub fn lower_to_claude(
    profile: &AgentProfile,
    _fm: &Frontmatter,
    body: &str,
) -> LoweredOutput {
    let eff = Effective::new(profile, &HarnessKind::Claude);
    let mut lossy = Vec::new();

    // Build the native frontmatter mapping
    let mut yaml = serde_yaml::Mapping::new();
    let yk = |s: &str| serde_yaml::Value::String(s.to_string());
    let yv = |s: &str| serde_yaml::Value::String(s.to_string());

    // name — exact
    if let Some(name) = &profile.name {
        yaml.insert(yk("name"), yv(name));
    }
    // description — exact
    if let Some(desc) = &profile.description {
        yaml.insert(yk("description"), yv(desc));
    }
    // model — exact (alias preserved; Claude resolves it)
    if let Some(model) = &profile.model {
        yaml.insert(yk("model"), yv(model));
    }
    // skills — exact (Claude reads skills natively from .agents/skills/)
    let skills = eff.skills();
    if !skills.is_empty() {
        let seq: serde_yaml::Value = serde_yaml::Value::Sequence(
            skills.iter().map(|s| yv(s)).collect(),
        );
        yaml.insert(yk("skills"), seq);
    }
    // tools — exact
    let tools = eff.tools();
    if !tools.is_empty() {
        let seq: serde_yaml::Value = serde_yaml::Value::Sequence(
            tools.iter().map(|s| yv(s)).collect(),
        );
        yaml.insert(yk("tools"), seq);
    }
    // disallowed-tools — exact
    let dt = eff.disallowed_tools();
    if !dt.is_empty() {
        let seq: serde_yaml::Value = serde_yaml::Value::Sequence(
            dt.iter().map(|s| yv(s)).collect(),
        );
        yaml.insert(yk("disallowed-tools"), seq);
    }

    // mcp-tools — exact (pass through raw from source)
    let mcp = &profile.mcp_tools;
    if !mcp.is_empty() {
        let seq: serde_yaml::Value = serde_yaml::Value::Sequence(
            mcp.iter().map(|s| yv(s)).collect(),
        );
        yaml.insert(yk("mcp-tools"), seq);
    }

    // effort — exact (passed as frontmatter hint; Claude reads it)
    if let Some(effort) = eff.effort() {
        yaml.insert(yk("effort"), yv(effort.claude_str()));
    }

    // --- Dropped / meridian-only fields ---
    let target = "Claude";
    if profile.approval.is_some() {
        lossy.push(LossyField {
            field: "approval".into(),
            target: target.into(),
            classification: Lossiness::Dropped,
        });
    }
    if profile.sandbox.is_some() {
        lossy.push(LossyField {
            field: "sandbox".into(),
            target: target.into(),
            classification: Lossiness::Dropped,
        });
    }
    if profile.mode.is_some() {
        lossy.push(LossyField {
            field: "mode".into(),
            target: target.into(),
            classification: Lossiness::Dropped,
        });
    }
    if profile.autocompact.is_some() {
        lossy.push(LossyField {
            field: "autocompact".into(),
            target: target.into(),
            classification: Lossiness::MeridianOnly,
        });
    }
    if !profile.model_policies.is_empty() {
        lossy.push(LossyField {
            field: "model-policies".into(),
            target: target.into(),
            classification: Lossiness::MeridianOnly,
        });
    }
    if !profile.fanout.is_empty() {
        lossy.push(LossyField {
            field: "fanout".into(),
            target: target.into(),
            classification: Lossiness::MeridianOnly,
        });
    }
    // harness: field is dropped (the native artifact's location IS the harness)
    // harness-overrides: merged above, then dropped

    // Serialize
    let yaml_str = if yaml.is_empty() {
        String::new()
    } else {
        let mut s = serde_yaml::to_string(&yaml).unwrap_or_default();
        if let Some(stripped) = s.strip_prefix("---\n") {
            s = stripped.to_string();
        }
        s
    };

    let out = if yaml.is_empty() && body.is_empty() {
        String::new()
    } else if yaml.is_empty() {
        body.to_string()
    } else {
        format!("---\n{}---\n{}", yaml_str, body)
    };

    LoweredOutput {
        bytes: out.into_bytes(),
        lossy_fields: lossy,
    }
}

// ---------------------------------------------------------------------------
// Codex native artifact (TOML)
// ---------------------------------------------------------------------------

/// Lower an agent profile to Codex-native TOML format.
///
/// Per agent-compilation-mapping.md V0 §5.4 and §10:
/// - Preserved: name, description, model, effort (as model_reasoning_effort),
///   sandbox (as sandbox_mode), approval (as approval_policy), body (as instructions)
/// - Dropped: skills (no native field), tools (no allowlist), disallowed-tools,
///   mcp-tools (approximate), mode, autocompact, model-policies, fanout
/// - Merged: harness-overrides.codex applied to top-level fields before lowering
pub fn lower_to_codex(
    profile: &AgentProfile,
    body: &str,
) -> LoweredOutput {
    let eff = Effective::new(profile, &HarnessKind::Codex);
    let mut lossy = Vec::new();
    let target = "Codex";

    let name = profile.name.as_deref().unwrap_or("");
    let description = profile.description.as_deref().unwrap_or("");
    let model = profile.model.as_deref().unwrap_or("");

    // Effort — exact (lowered to model_reasoning_effort)
    let effort_str = eff.effort().map(|e| e.as_str()).unwrap_or("");

    // Sandbox — exact
    let sandbox_str = eff.sandbox().map(|s| s.as_str()).unwrap_or("");

    // Approval — exact (lowered to approval_policy)
    let approval_policy = eff.approval().map(|a| {
        use crate::compiler::agents::ApprovalMode;
        match a {
            ApprovalMode::Default => "",
            ApprovalMode::Auto => "on-request",
            ApprovalMode::Confirm => "untrusted",
            ApprovalMode::Yolo => "bypass",
        }
    }).unwrap_or("");

    // Dropped fields
    let skills = eff.skills();
    if !skills.is_empty() {
        lossy.push(LossyField {
            field: "skills".into(),
            target: target.into(),
            classification: Lossiness::Dropped,
        });
    }
    let tools = eff.tools();
    if !tools.is_empty() {
        lossy.push(LossyField {
            field: "tools".into(),
            target: target.into(),
            classification: Lossiness::Dropped,
        });
    }
    let dt = eff.disallowed_tools();
    if !dt.is_empty() {
        lossy.push(LossyField {
            field: "disallowed-tools".into(),
            target: target.into(),
            classification: Lossiness::Dropped,
        });
    }
    if !profile.mcp_tools.is_empty() {
        lossy.push(LossyField {
            field: "mcp-tools".into(),
            target: target.into(),
            classification: Lossiness::Approximate { note: "Codex uses -c mcp.servers.<name>.command" },
        });
    }
    if profile.mode.is_some() {
        lossy.push(LossyField {
            field: "mode".into(),
            target: target.into(),
            classification: Lossiness::Dropped,
        });
    }
    if profile.autocompact.is_some() {
        lossy.push(LossyField {
            field: "autocompact".into(),
            target: target.into(),
            classification: Lossiness::MeridianOnly,
        });
    }
    if !profile.model_policies.is_empty() {
        lossy.push(LossyField {
            field: "model-policies".into(),
            target: target.into(),
            classification: Lossiness::MeridianOnly,
        });
    }
    if !profile.fanout.is_empty() {
        lossy.push(LossyField {
            field: "fanout".into(),
            target: target.into(),
            classification: Lossiness::MeridianOnly,
        });
    }

    // Build TOML
    let mut out = String::new();
    out.push_str("[agent]\n");
    out.push_str(&format!("name = {}\n", toml_str(name)));
    if !description.is_empty() {
        out.push_str(&format!("description = {}\n", toml_str(description)));
    }
    if !model.is_empty() {
        out.push_str(&format!("model = {}\n", toml_str(model)));
    }

    let has_config = !effort_str.is_empty() || !sandbox_str.is_empty() || !approval_policy.is_empty();
    if has_config {
        out.push_str("\n[agent.config]\n");
        if !effort_str.is_empty() {
            out.push_str(&format!("model_reasoning_effort = {}\n", toml_str(effort_str)));
        }
        if !sandbox_str.is_empty() {
            out.push_str(&format!("sandbox_mode = {}\n", toml_str(sandbox_str)));
        }
        if !approval_policy.is_empty() {
            out.push_str(&format!("approval_policy = {}\n", toml_str(approval_policy)));
        }
    }

    if !body.is_empty() {
        out.push_str("\n[agent.instructions]\n");
        out.push_str(&format!("content = \"\"\"\n{}\n\"\"\"\n", body.trim_end()));
    }

    LoweredOutput {
        bytes: out.into_bytes(),
        lossy_fields: lossy,
    }
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

// ---------------------------------------------------------------------------
// OpenCode native artifact
// ---------------------------------------------------------------------------

/// Lower an agent profile to OpenCode-native markdown format.
///
/// Per agent-compilation-mapping.md V0 §5.5 and §10:
/// - Preserved: name, description, model (normalized to provider/model), mode
///   (approximate — same field name), body
/// - Dropped: most policy fields (approval, sandbox, tools, disallowed-tools,
///   effort, mcp-tools, autocompact)
/// - Meridian-only: model-policies, fanout
pub fn lower_to_opencode(
    profile: &AgentProfile,
    body: &str,
) -> LoweredOutput {
    let eff = Effective::new(profile, &HarnessKind::OpenCode);
    let mut lossy = Vec::new();
    let target = "OpenCode";

    let mut yaml = serde_yaml::Mapping::new();
    let yk = |s: &str| serde_yaml::Value::String(s.to_string());
    let yv = |s: &str| serde_yaml::Value::String(s.to_string());

    if let Some(name) = &profile.name {
        yaml.insert(yk("name"), yv(name));
    }
    if let Some(desc) = &profile.description {
        yaml.insert(yk("description"), yv(desc));
    }
    if let Some(model) = &profile.model {
        // OpenCode uses provider/model format — pass through alias as-is for V0;
        // full resolution requires the model catalog (out of scope for Phase 3).
        yaml.insert(yk("model"), yv(model));
    }
    // mode — approximate (OpenCode has a mode concept: primary/subagent)
    if let Some(mode) = &profile.mode {
        yaml.insert(yk("mode"), yv(mode.as_str()));
        lossy.push(LossyField {
            field: "mode".into(),
            target: target.into(),
            classification: Lossiness::Approximate { note: "OpenCode uses the same mode concept" },
        });
    }

    // Dropped fields
    if eff.approval().is_some() {
        lossy.push(LossyField { field: "approval".into(), target: target.into(), classification: Lossiness::Dropped });
    }
    if eff.sandbox().is_some() {
        lossy.push(LossyField { field: "sandbox".into(), target: target.into(), classification: Lossiness::Dropped });
    }
    if !eff.tools().is_empty() {
        lossy.push(LossyField { field: "tools".into(), target: target.into(), classification: Lossiness::Dropped });
    }
    if !eff.disallowed_tools().is_empty() {
        lossy.push(LossyField { field: "disallowed-tools".into(), target: target.into(), classification: Lossiness::Dropped });
    }
    if eff.effort().is_some() {
        lossy.push(LossyField {
            field: "effort".into(),
            target: target.into(),
            classification: Lossiness::Approximate { note: "effort maps to --variant on subprocess only" },
        });
    }
    if !profile.mcp_tools.is_empty() {
        lossy.push(LossyField {
            field: "mcp-tools".into(),
            target: target.into(),
            classification: Lossiness::Approximate { note: "mcp-tools on subprocess errors; streaming uses session payload" },
        });
    }
    if profile.autocompact.is_some() {
        lossy.push(LossyField { field: "autocompact".into(), target: target.into(), classification: Lossiness::MeridianOnly });
    }
    if !profile.model_policies.is_empty() {
        lossy.push(LossyField { field: "model-policies".into(), target: target.into(), classification: Lossiness::MeridianOnly });
    }
    if !profile.fanout.is_empty() {
        lossy.push(LossyField { field: "fanout".into(), target: target.into(), classification: Lossiness::MeridianOnly });
    }

    // Serialize
    let yaml_str = if yaml.is_empty() {
        String::new()
    } else {
        let mut s = serde_yaml::to_string(&yaml).unwrap_or_default();
        if let Some(stripped) = s.strip_prefix("---\n") {
            s = stripped.to_string();
        }
        s
    };

    let out = if yaml.is_empty() {
        body.to_string()
    } else {
        format!("---\n{}---\n{}", yaml_str, body)
    };

    LoweredOutput {
        bytes: out.into_bytes(),
        lossy_fields: lossy,
    }
}

// ---------------------------------------------------------------------------
// Pi native artifact
// ---------------------------------------------------------------------------

/// Lower an agent profile to Pi-native markdown format.
///
/// Pi's format is similar to OpenCode: markdown + YAML frontmatter with a
/// minimal subset of fields. Per agent-compilation-mapping.md §6, all policy
/// fields are dropped.
pub fn lower_to_pi(
    profile: &AgentProfile,
    body: &str,
) -> LoweredOutput {
    let mut lossy = Vec::new();
    let target = "Pi";

    let mut yaml = serde_yaml::Mapping::new();
    let yk = |s: &str| serde_yaml::Value::String(s.to_string());
    let yv = |s: &str| serde_yaml::Value::String(s.to_string());

    if let Some(name) = &profile.name {
        yaml.insert(yk("name"), yv(name));
    }
    if let Some(desc) = &profile.description {
        yaml.insert(yk("description"), yv(desc));
    }
    if let Some(model) = &profile.model {
        yaml.insert(yk("model"), yv(model));
    }
    // mode — approximate
    if let Some(mode) = &profile.mode {
        yaml.insert(yk("mode"), yv(mode.as_str()));
        lossy.push(LossyField {
            field: "mode".into(),
            target: target.into(),
            classification: Lossiness::Approximate { note: "Pi may use the same mode concept" },
        });
    }

    // Everything else is dropped
    let eff = Effective::new(profile, &HarnessKind::Pi);
    if eff.approval().is_some() {
        lossy.push(LossyField { field: "approval".into(), target: target.into(), classification: Lossiness::Dropped });
    }
    if eff.sandbox().is_some() {
        lossy.push(LossyField { field: "sandbox".into(), target: target.into(), classification: Lossiness::Dropped });
    }
    if !eff.tools().is_empty() {
        lossy.push(LossyField { field: "tools".into(), target: target.into(), classification: Lossiness::Dropped });
    }
    if !eff.disallowed_tools().is_empty() {
        lossy.push(LossyField { field: "disallowed-tools".into(), target: target.into(), classification: Lossiness::Dropped });
    }
    if eff.effort().is_some() {
        lossy.push(LossyField {
            field: "effort".into(),
            target: target.into(),
            classification: Lossiness::Approximate { note: "Pi effort semantics unverified" },
        });
    }
    if profile.autocompact.is_some() {
        lossy.push(LossyField { field: "autocompact".into(), target: target.into(), classification: Lossiness::MeridianOnly });
    }
    if !profile.model_policies.is_empty() {
        lossy.push(LossyField { field: "model-policies".into(), target: target.into(), classification: Lossiness::MeridianOnly });
    }
    if !profile.fanout.is_empty() {
        lossy.push(LossyField { field: "fanout".into(), target: target.into(), classification: Lossiness::MeridianOnly });
    }

    let yaml_str = if yaml.is_empty() {
        String::new()
    } else {
        let mut s = serde_yaml::to_string(&yaml).unwrap_or_default();
        if let Some(stripped) = s.strip_prefix("---\n") {
            s = stripped.to_string();
        }
        s
    };

    let out = if yaml.is_empty() {
        body.to_string()
    } else {
        format!("---\n{}---\n{}", yaml_str, body)
    };

    LoweredOutput {
        bytes: out.into_bytes(),
        lossy_fields: lossy,
    }
}

// ---------------------------------------------------------------------------
// Dispatch: lower for a given harness
// ---------------------------------------------------------------------------

/// Lower an agent to the native format for the given harness.
///
/// Returns `None` for unknown harnesses (should not happen if the profile was
/// validated, but guards against future harness additions).
pub fn lower_for_harness(
    harness: &HarnessKind,
    profile: &AgentProfile,
    fm: &Frontmatter,
    body: &str,
) -> LoweredOutput {
    match harness {
        HarnessKind::Claude => lower_to_claude(profile, fm, body),
        HarnessKind::Codex => lower_to_codex(profile, body),
        HarnessKind::OpenCode => lower_to_opencode(profile, body),
        HarnessKind::Pi => lower_to_pi(profile, body),
    }
}

/// Collect lossiness diagnostics from a lowered output and push them as
/// `AgentDiagnostic::DroppedField` entries.
pub fn collect_lossiness_diags(
    output: &LoweredOutput,
    diags: &mut Vec<AgentDiagnostic>,
) {
    for lf in &output.lossy_fields {
        if matches!(lf.classification, Lossiness::Dropped | Lossiness::MeridianOnly) {
            diags.push(AgentDiagnostic::DroppedField {
                field: lf.field.clone(),
                target: lf.target.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::agents::{parse_agent_content};

    fn profile_from(content: &str) -> (AgentProfile, Frontmatter, Vec<AgentDiagnostic>) {
        let mut diags = Vec::new();
        let (profile, fm) = parse_agent_content(content, &mut diags).unwrap();
        (profile, fm, diags)
    }

    // --- 3.1: Meridian pass-through ---

    #[test]
    fn meridian_lowering_is_identity() {
        let src = "---\nname: coder\nharness: claude\n---\n# Body";
        let out = lower_to_meridian(src);
        assert_eq!(out.bytes, src.as_bytes());
        assert!(out.lossy_fields.is_empty());
    }

    // --- 3.3: Claude lowering ---

    #[test]
    fn claude_lowering_preserves_name_description_model_skills_tools_body() {
        let content = "---\nname: coder\ndescription: Code impl agent\nmodel: gpt55\nharness: claude\nskills: [dev-principles]\ntools: [Bash, Write]\n---\n# Coder\nYou write code.";
        let (profile, fm, _) = profile_from(content);
        let body = fm.body();
        let out = lower_to_claude(&profile, &fm, body);
        let text = String::from_utf8(out.bytes).unwrap();
        assert!(text.contains("name: coder"), "name missing: {text}");
        assert!(text.contains("description: Code impl agent"), "desc missing");
        assert!(text.contains("model: gpt55"), "model missing");
        assert!(text.contains("skills"), "skills missing");
        assert!(text.contains("tools"), "tools missing");
        assert!(text.contains("# Coder"), "body missing");
    }

    #[test]
    fn claude_lowering_drops_approval_sandbox_mode_autocompact() {
        let content = "---\nname: coder\nharness: claude\napproval: auto\nsandbox: read-only\nmode: subagent\nautocompact: 50\n---\n# Body";
        let (profile, fm, _) = profile_from(content);
        let out = lower_to_claude(&profile, &fm, fm.body());
        let text = String::from_utf8(out.bytes).unwrap();
        assert!(!text.contains("approval:"), "approval leaked: {text}");
        assert!(!text.contains("sandbox:"), "sandbox leaked: {text}");
        assert!(!text.contains("autocompact:"), "autocompact leaked: {text}");
        // Lossiness should report dropped fields
        let dropped: Vec<_> = out.lossy_fields.iter().map(|f| f.field.as_str()).collect();
        assert!(dropped.contains(&"approval"), "approval not in lossy: {dropped:?}");
        assert!(dropped.contains(&"sandbox"), "sandbox not in lossy: {dropped:?}");
        assert!(dropped.contains(&"autocompact"), "autocompact not in lossy: {dropped:?}");
    }

    #[test]
    fn claude_harness_override_applied_before_lowering() {
        let content = "---\nname: r\nharness: claude\nskills: [base-skill]\nharness-overrides:\n  claude:\n    skills: [override-skill]\n---\n# body";
        let (profile, fm, _) = profile_from(content);
        let out = lower_to_claude(&profile, &fm, fm.body());
        let text = String::from_utf8(out.bytes).unwrap();
        assert!(text.contains("override-skill"), "override not applied: {text}");
        assert!(!text.contains("base-skill"), "base skill not overridden: {text}");
    }

    #[test]
    fn claude_meridian_only_fields_dropped() {
        let content = "---\nname: r\nharness: claude\nmodel-policies:\n  - match:\n      model: gpt55\n    override:\n      harness: codex\nfanout:\n  - alias: opus\n---\n# body";
        let (profile, fm, _) = profile_from(content);
        let out = lower_to_claude(&profile, &fm, fm.body());
        let text = String::from_utf8(out.bytes).unwrap();
        assert!(!text.contains("model-policies:"), "model-policies leaked: {text}");
        assert!(!text.contains("fanout:"), "fanout leaked: {text}");
        let meridian_only: Vec<_> = out.lossy_fields.iter()
            .filter(|f| matches!(f.classification, Lossiness::MeridianOnly))
            .map(|f| f.field.as_str())
            .collect();
        assert!(meridian_only.contains(&"model-policies"));
        assert!(meridian_only.contains(&"fanout"));
    }

    // --- 3.3: Codex lowering ---

    #[test]
    fn codex_lowering_produces_toml_with_agent_section() {
        let content = "---\nname: coder\ndescription: Code agent\nmodel: gpt55\nharness: codex\neffort: high\nsandbox: workspace-write\napproval: auto\n---\n# Coder\nYou code.";
        let (profile, fm, _) = profile_from(content);
        let out = lower_to_codex(&profile, fm.body());
        let text = String::from_utf8(out.bytes).unwrap();
        assert!(text.contains("[agent]"), "no [agent] section: {text}");
        assert!(text.contains("name = \"coder\""), "name missing");
        assert!(text.contains("model = \"gpt55\""), "model missing");
        assert!(text.contains("[agent.config]"), "no config section");
        assert!(text.contains("model_reasoning_effort = \"high\""), "effort missing");
        assert!(text.contains("sandbox_mode = \"workspace-write\""), "sandbox missing");
        assert!(text.contains("approval_policy = \"on-request\""), "approval missing");
        assert!(text.contains("[agent.instructions]"), "no instructions section");
    }

    #[test]
    fn codex_lowering_drops_skills_and_tools() {
        let content = "---\nname: r\nharness: codex\nskills: [review]\ntools: [Bash]\ndisallowed-tools: [Agent]\n---\n# body";
        let (profile, fm, _) = profile_from(content);
        let out = lower_to_codex(&profile, fm.body());
        let dropped: Vec<_> = out.lossy_fields.iter()
            .filter(|f| matches!(f.classification, Lossiness::Dropped))
            .map(|f| f.field.as_str())
            .collect();
        assert!(dropped.contains(&"skills"));
        assert!(dropped.contains(&"tools"));
        assert!(dropped.contains(&"disallowed-tools"));
    }

    #[test]
    fn codex_harness_override_applied() {
        let content = "---\nname: r\nharness: codex\neffort: low\nharness-overrides:\n  codex:\n    effort: high\n    sandbox: workspace-write\n---\n# body";
        let (profile, fm, _) = profile_from(content);
        let out = lower_to_codex(&profile, fm.body());
        let text = String::from_utf8(out.bytes).unwrap();
        assert!(text.contains("model_reasoning_effort = \"high\""), "override not applied: {text}");
        assert!(text.contains("sandbox_mode = \"workspace-write\""), "sandbox override not applied: {text}");
    }

    // --- 3.3: OpenCode lowering ---

    #[test]
    fn opencode_lowering_preserves_name_description_model_mode() {
        let content = "---\nname: r\ndescription: Reviewer\nmodel: gpt55\nmode: primary\nharness: opencode\n---\n# Reviewer\nbody";
        let (profile, fm, _) = profile_from(content);
        let out = lower_to_opencode(&profile, fm.body());
        let text = String::from_utf8(out.bytes).unwrap();
        assert!(text.contains("name: r"), "name missing");
        assert!(text.contains("description: Reviewer"), "desc missing");
        assert!(text.contains("model: gpt55"), "model missing");
        assert!(text.contains("mode: primary"), "mode missing");
    }

    // --- 3.3: Pi lowering ---

    #[test]
    fn pi_lowering_preserves_name_description_model() {
        let content = "---\nname: pi-agent\ndescription: Pi agent\nmodel: gpt55\nharness: pi\n---\n# Pi\nbody";
        let (profile, fm, _) = profile_from(content);
        let out = lower_to_pi(&profile, fm.body());
        let text = String::from_utf8(out.bytes).unwrap();
        assert!(text.contains("name: pi-agent"), "name missing");
        assert!(text.contains("description: Pi agent"), "desc missing");
    }

    // --- 3.3: Dispatch ---

    #[test]
    fn lower_for_harness_dispatches_correctly() {
        let content = "---\nname: coder\nmodel: gpt55\nharness: claude\n---\n# body";
        let (profile, fm, _) = profile_from(content);
        let body = fm.body().to_string();
        let out = lower_for_harness(&HarnessKind::Claude, &profile, &fm, &body);
        let text = String::from_utf8(out.bytes).unwrap();
        assert!(text.contains("---"), "not markdown format");

        let content2 = "---\nname: coder\nmodel: gpt55\nharness: codex\n---\n# body";
        let (profile2, fm2, _) = profile_from(content2);
        let body2 = fm2.body().to_string();
        let out2 = lower_for_harness(&HarnessKind::Codex, &profile2, &fm2, &body2);
        let text2 = String::from_utf8(out2.bytes).unwrap();
        assert!(text2.contains("[agent]"), "not TOML format");
    }
}
