/// Agent-profile schema, parser, and validation.
///
/// Parses agent markdown frontmatter into strongly-typed [`AgentProfile`] fields.
/// Used by the dual-surface compilation pipeline to:
/// - Validate agent profiles at compile time
/// - Route agents to the correct harness-native output surface
/// - Report lossiness diagnostics when fields cannot be expressed in a target format
pub mod lower;

use serde_yaml::Value;

use crate::frontmatter::{Frontmatter, FrontmatterError};

// ---------------------------------------------------------------------------
// Field enums
// ---------------------------------------------------------------------------

/// Agent execution mode — how the agent is launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentMode {
    Primary,
    Subagent,
}

impl AgentMode {
    pub fn as_str(&self) -> &str {
        match self {
            AgentMode::Primary => "primary",
            AgentMode::Subagent => "subagent",
        }
    }
}

impl std::fmt::Display for AgentMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Known harness execution targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessKind {
    Claude,
    Codex,
    OpenCode,
    Pi,
}

impl HarnessKind {
    pub fn all() -> &'static [Self] {
        &[Self::Claude, Self::Codex, Self::OpenCode, Self::Pi]
    }

    /// Parse from a frontmatter string value.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::OpenCode),
            "pi" => Some(Self::Pi),
            _ => None,
        }
    }

    /// Target directory root for harness-native artifacts.
    pub fn target_dir(&self) -> &str {
        match self {
            Self::Claude => ".claude",
            Self::Codex => ".codex",
            Self::OpenCode => ".opencode",
            Self::Pi => ".pi",
        }
    }
}

/// Approval policy field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalMode {
    Default,
    Auto,
    Confirm,
    Yolo,
}

impl ApprovalMode {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "auto" => Some(Self::Auto),
            "confirm" => Some(Self::Confirm),
            "yolo" => Some(Self::Yolo),
            _ => None,
        }
    }
}

/// Sandbox mode field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxMode {
    Default,
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl SandboxMode {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "read-only" => Some(Self::ReadOnly),
            "workspace-write" => Some(Self::WorkspaceWrite),
            "danger-full-access" => Some(Self::DangerFullAccess),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Default => "default",
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

/// Effort level field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffortLevel {
    Low,
    Medium,
    High,
    XHigh,
}

impl EffortLevel {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "max" => Some(Self::XHigh),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }

    /// Normalized value for Claude ("xhigh" → "max").
    pub fn claude_str(&self) -> &str {
        match self {
            Self::XHigh => "max",
            other => other.as_str(),
        }
    }
}

// ---------------------------------------------------------------------------
// Override table types
// ---------------------------------------------------------------------------

/// A set of overridable field values for one harness or model override entry.
/// Only fields explicitly specified in the override block are present.
#[derive(Debug, Clone, Default)]
pub struct OverrideFields {
    pub effort: Option<EffortLevel>,
    pub autocompact: Option<u32>,
    pub approval: Option<ApprovalMode>,
    pub sandbox: Option<SandboxMode>,
    pub skills: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,
    pub mcp_tools: Option<Vec<String>>,
}

/// Per-harness override table (`harness-overrides:`).
#[derive(Debug, Clone, Default)]
pub struct HarnessOverrides {
    pub claude: Option<OverrideFields>,
    pub codex: Option<OverrideFields>,
    pub opencode: Option<OverrideFields>,
    pub pi: Option<OverrideFields>,
}

impl HarnessOverrides {
    pub fn get(&self, harness: &HarnessKind) -> Option<&OverrideFields> {
        match harness {
            HarnessKind::Claude => self.claude.as_ref(),
            HarnessKind::Codex => self.codex.as_ref(),
            HarnessKind::OpenCode => self.opencode.as_ref(),
            HarnessKind::Pi => self.pi.as_ref(),
        }
    }
}

/// Marker for a validated `model-policies:` entry.
///
/// Per the spec (D43), model-policies are consumed by Meridian at runtime.
/// Mars parses them at compile time only for validation and preservation.
#[derive(Debug, Clone)]
pub struct ModelPolicyEntry;

/// Marker for a validated fanout inventory entry (`fanout:`).
///
/// Fanout is metadata-only (D43): it never gains lowering behavior.
/// Mars parses it for validation and preservation; no harness-native artifact
/// receives fanout entries.
#[derive(Debug, Clone)]
pub struct FanoutEntry;

// ---------------------------------------------------------------------------
// AgentProfile — the fully parsed frontmatter
// ---------------------------------------------------------------------------

/// Strongly-typed representation of an agent profile's frontmatter.
///
/// Parsed from YAML frontmatter by [`parse_agent_profile`].
/// Used for:
/// - Compile-time validation (mode values, non-overridable fields in overrides)
/// - Dual-surface routing (harness → output target)
/// - Per-target lowering (field lowering per agent-compilation-mapping.md)
#[derive(Debug, Clone)]
pub struct AgentProfile {
    // --- Identity fields ---
    pub name: Option<String>,
    pub description: Option<String>,

    // --- Routing fields ---
    pub harness: Option<HarnessKind>,

    // --- Model fields ---
    pub model: Option<String>,

    // --- Runtime policy fields ---
    pub mode: Option<AgentMode>,
    pub approval: Option<ApprovalMode>,
    pub sandbox: Option<SandboxMode>,
    pub effort: Option<EffortLevel>,
    pub autocompact: Option<u32>,

    // --- Tool fields ---
    pub skills: Vec<String>,
    pub tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub mcp_tools: Vec<String>,

    // --- Override tables ---
    pub harness_overrides: HarnessOverrides,
    pub model_policies: Vec<ModelPolicyEntry>,
    pub fanout: Vec<FanoutEntry>,
}

// ---------------------------------------------------------------------------
// Validation warnings/errors
// ---------------------------------------------------------------------------

/// A validation finding from agent profile parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentDiagnostic {
    /// Field value is not in the allowed set.
    InvalidFieldValue {
        field: String,
        value: String,
        allowed: &'static str,
    },
    /// Deprecated `models:` field was found (use `model-overrides:` instead).
    LegacyModelsField,
    /// Unknown harness name — not one of claude/codex/opencode/pi.
    UnknownHarness { value: String },
    /// Non-overridable field appears inside an override block.
    NonOverridableFieldInOverride { field: String, table: String },
}

impl AgentDiagnostic {
    pub fn is_error(&self) -> bool {
        matches!(self, AgentDiagnostic::InvalidFieldValue { .. })
    }

    pub fn message(&self) -> String {
        match self {
            AgentDiagnostic::InvalidFieldValue {
                field,
                value,
                allowed,
            } => {
                format!("agent field `{field}` has invalid value `{value}`; allowed: {allowed}")
            }
            AgentDiagnostic::LegacyModelsField => {
                "agent uses deprecated `models:` field; rename to `model-overrides:`".to_string()
            }
            AgentDiagnostic::UnknownHarness { value } => {
                format!("unknown harness `{value}`; known: claude, codex, opencode, pi")
            }
            AgentDiagnostic::NonOverridableFieldInOverride { field, table } => {
                format!("field `{field}` is not overridable; remove from `{table}`")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Non-overridable field names (compile error if inside an override block)
// ---------------------------------------------------------------------------

const NON_OVERRIDABLE: &[&str] = &[
    "name",
    "description",
    "model",
    "harness",
    "mode",
    "model-overrides",
    "harness-overrides",
];

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn yaml_str_list(val: &Value) -> Vec<String> {
    match val {
        Value::Sequence(seq) => seq
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_owned)
            .collect(),
        Value::String(s) => vec![s.clone()],
        _ => vec![],
    }
}

fn parse_override_fields(
    mapping: &serde_yaml::Mapping,
    table_name: &str,
    diags: &mut Vec<AgentDiagnostic>,
) -> OverrideFields {
    let mut out = OverrideFields::default();

    for (k, v) in mapping {
        let key = match k.as_str() {
            Some(s) => s,
            None => continue,
        };

        if NON_OVERRIDABLE.contains(&key) {
            diags.push(AgentDiagnostic::NonOverridableFieldInOverride {
                field: key.to_string(),
                table: table_name.to_string(),
            });
            continue;
        }

        match key {
            "effort" => {
                if let Some(s) = v.as_str() {
                    if let Some(e) = EffortLevel::from_str(s) {
                        out.effort = Some(e);
                    } else {
                        diags.push(AgentDiagnostic::InvalidFieldValue {
                            field: format!("{table_name}.effort"),
                            value: s.to_string(),
                            allowed: "low, medium, high, xhigh",
                        });
                    }
                }
            }
            "autocompact" => {
                if let Some(n) = v.as_u64() {
                    match u32::try_from(n) {
                        Ok(v32) => out.autocompact = Some(v32),
                        Err(_) => diags.push(AgentDiagnostic::InvalidFieldValue {
                            field: format!("{table_name}.autocompact"),
                            value: n.to_string(),
                            allowed: "integer 0–4294967295",
                        }),
                    }
                }
            }
            "approval" => {
                if let Some(s) = v.as_str() {
                    if let Some(a) = ApprovalMode::from_str(s) {
                        out.approval = Some(a);
                    } else {
                        diags.push(AgentDiagnostic::InvalidFieldValue {
                            field: format!("{table_name}.approval"),
                            value: s.to_string(),
                            allowed: "default, auto, confirm, yolo",
                        });
                    }
                }
            }
            "sandbox" => {
                if let Some(s) = v.as_str() {
                    if let Some(sb) = SandboxMode::from_str(s) {
                        out.sandbox = Some(sb);
                    } else {
                        diags.push(AgentDiagnostic::InvalidFieldValue {
                            field: format!("{table_name}.sandbox"),
                            value: s.to_string(),
                            allowed: "default, read-only, workspace-write, danger-full-access",
                        });
                    }
                }
            }
            "skills" => {
                out.skills = Some(yaml_str_list(v));
            }
            "tools" => {
                out.tools = Some(yaml_str_list(v));
            }
            "disallowed-tools" => {
                out.disallowed_tools = Some(yaml_str_list(v));
            }
            "mcp-tools" => {
                out.mcp_tools = Some(yaml_str_list(v));
            }
            _ => {
                // Unknown override field — tolerate (forward compat).
            }
        }
    }

    out
}

fn parse_harness_overrides(val: &Value, diags: &mut Vec<AgentDiagnostic>) -> HarnessOverrides {
    let mut out = HarnessOverrides::default();
    let Some(mapping) = val.as_mapping() else {
        return out;
    };

    for (k, v) in mapping {
        let harness_name = match k.as_str() {
            Some(s) => s,
            None => continue,
        };
        let sub_mapping = match v.as_mapping() {
            Some(m) => m,
            None => continue,
        };
        let table_name = format!("harness-overrides.{harness_name}");
        let fields = parse_override_fields(sub_mapping, &table_name, diags);
        match harness_name {
            "claude" => out.claude = Some(fields),
            "codex" => out.codex = Some(fields),
            "opencode" => out.opencode = Some(fields),
            "pi" => out.pi = Some(fields),
            other => {
                diags.push(AgentDiagnostic::UnknownHarness {
                    value: other.to_string(),
                });
            }
        }
    }

    out
}

fn parse_model_policies(val: &Value) -> Vec<ModelPolicyEntry> {
    match val {
        Value::Sequence(seq) => seq.iter().map(|_| ModelPolicyEntry).collect(),
        _ => vec![],
    }
}

fn parse_fanout(val: &Value) -> Vec<FanoutEntry> {
    match val {
        Value::Sequence(seq) => seq.iter().map(|_| FanoutEntry).collect(),
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse an agent profile from a [`Frontmatter`].
///
/// Collects diagnostics without failing — the caller decides whether errors
/// are fatal. The parsed [`AgentProfile`] is always returned even when there
/// are validation errors; invalid fields are skipped (omitted from the profile).
pub fn parse_agent_profile(fm: &Frontmatter, diags: &mut Vec<AgentDiagnostic>) -> AgentProfile {
    let name = fm.name().map(str::to_owned);
    let description = fm
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned);

    // harness:
    let harness = fm.get("harness").and_then(Value::as_str).and_then(|s| {
        if let Some(h) = HarnessKind::from_str(s) {
            Some(h)
        } else {
            diags.push(AgentDiagnostic::UnknownHarness {
                value: s.to_string(),
            });
            None
        }
    });

    // model:
    let model = fm.get("model").and_then(Value::as_str).map(str::to_owned);

    // mode:
    let mode = fm
        .get("mode")
        .and_then(Value::as_str)
        .and_then(|s| match s {
            "primary" => Some(AgentMode::Primary),
            "subagent" => Some(AgentMode::Subagent),
            other => {
                diags.push(AgentDiagnostic::InvalidFieldValue {
                    field: "mode".to_string(),
                    value: other.to_string(),
                    allowed: "primary, subagent",
                });
                None
            }
        });

    // approval:
    let approval = fm.get("approval").and_then(Value::as_str).and_then(|s| {
        if let Some(a) = ApprovalMode::from_str(s) {
            Some(a)
        } else {
            diags.push(AgentDiagnostic::InvalidFieldValue {
                field: "approval".to_string(),
                value: s.to_string(),
                allowed: "default, auto, confirm, yolo",
            });
            None
        }
    });

    // sandbox:
    let sandbox = fm.get("sandbox").and_then(Value::as_str).and_then(|s| {
        if let Some(sb) = SandboxMode::from_str(s) {
            Some(sb)
        } else {
            diags.push(AgentDiagnostic::InvalidFieldValue {
                field: "sandbox".to_string(),
                value: s.to_string(),
                allowed: "default, read-only, workspace-write, danger-full-access",
            });
            None
        }
    });

    // effort:
    let effort = fm.get("effort").and_then(Value::as_str).and_then(|s| {
        if let Some(e) = EffortLevel::from_str(s) {
            Some(e)
        } else {
            diags.push(AgentDiagnostic::InvalidFieldValue {
                field: "effort".to_string(),
                value: s.to_string(),
                allowed: "low, medium, high, xhigh",
            });
            None
        }
    });

    // autocompact:
    let autocompact =
        fm.get("autocompact")
            .and_then(Value::as_u64)
            .and_then(|n| match u32::try_from(n) {
                Ok(v32) => Some(v32),
                Err(_) => {
                    diags.push(AgentDiagnostic::InvalidFieldValue {
                        field: "autocompact".to_string(),
                        value: n.to_string(),
                        allowed: "integer 0–4294967295",
                    });
                    None
                }
            });

    // skills/tools/disallowed-tools/mcp-tools:
    let skills = fm.skills();
    let tools = fm.get("tools").map(yaml_str_list).unwrap_or_default();
    let disallowed_tools = fm
        .get("disallowed-tools")
        .map(yaml_str_list)
        .unwrap_or_default();
    let mcp_tools = fm.get("mcp-tools").map(yaml_str_list).unwrap_or_default();

    // harness-overrides:
    let harness_overrides = fm
        .get("harness-overrides")
        .map(|v| parse_harness_overrides(v, diags))
        .unwrap_or_default();

    // model-policies:
    let model_policies = fm
        .get("model-policies")
        .map(parse_model_policies)
        .unwrap_or_default();

    // fanout:
    let fanout = fm.get("fanout").map(parse_fanout).unwrap_or_default();

    // Legacy models: field
    if fm.get("models").is_some() {
        diags.push(AgentDiagnostic::LegacyModelsField);
    }

    AgentProfile {
        name,
        description,
        harness,
        model,
        mode,
        approval,
        sandbox,
        effort,
        autocompact,
        skills,
        tools,
        disallowed_tools,
        mcp_tools,
        harness_overrides,
        model_policies,
        fanout,
    }
}

/// Parse an agent profile from raw markdown content.
///
/// Convenience wrapper over [`parse_agent_profile`].
pub fn parse_agent_content(
    content: &str,
    diags: &mut Vec<AgentDiagnostic>,
) -> Result<(AgentProfile, Frontmatter), FrontmatterError> {
    let fm = Frontmatter::parse(content)?;
    let profile = parse_agent_profile(&fm, diags);
    Ok((profile, fm))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::Frontmatter;

    fn parse(content: &str) -> (AgentProfile, Vec<AgentDiagnostic>) {
        let fm = Frontmatter::parse(content).unwrap();
        let mut diags = Vec::new();
        let profile = parse_agent_profile(&fm, &mut diags);
        (profile, diags)
    }

    // --- 3.1: Basic field parsing ---

    #[test]
    fn parses_name_and_description() {
        let (p, diags) = parse("---\nname: coder\ndescription: Code agent\n---\n# Body");
        assert!(diags.is_empty());
        assert_eq!(p.name.as_deref(), Some("coder"));
        assert_eq!(p.description.as_deref(), Some("Code agent"));
    }

    #[test]
    fn parses_mode_primary() {
        let (p, diags) = parse("---\nmode: primary\n---\n");
        assert!(diags.is_empty());
        assert_eq!(p.mode, Some(AgentMode::Primary));
    }

    #[test]
    fn parses_mode_subagent() {
        let (p, diags) = parse("---\nmode: subagent\n---\n");
        assert!(diags.is_empty());
        assert_eq!(p.mode, Some(AgentMode::Subagent));
    }

    #[test]
    fn invalid_mode_produces_diagnostic() {
        let (p, diags) = parse("---\nmode: invalid\n---\n");
        assert_eq!(p.mode, None);
        assert_eq!(diags.len(), 1);
        assert!(
            matches!(&diags[0], AgentDiagnostic::InvalidFieldValue { field, .. } if field == "mode")
        );
    }

    #[test]
    fn parses_harness_claude() {
        let (p, diags) = parse("---\nharness: claude\n---\n");
        assert!(diags.is_empty());
        assert_eq!(p.harness, Some(HarnessKind::Claude));
    }

    #[test]
    fn parses_harness_codex() {
        let (p, diags) = parse("---\nharness: codex\n---\n");
        assert!(diags.is_empty());
        assert_eq!(p.harness, Some(HarnessKind::Codex));
    }

    #[test]
    fn parses_harness_opencode() {
        let (p, diags) = parse("---\nharness: opencode\n---\n");
        assert!(diags.is_empty());
        assert_eq!(p.harness, Some(HarnessKind::OpenCode));
    }

    #[test]
    fn unknown_harness_produces_diagnostic() {
        let (p, diags) = parse("---\nharness: unknown\n---\n");
        assert_eq!(p.harness, None);
        assert_eq!(diags.len(), 1);
        assert!(
            matches!(&diags[0], AgentDiagnostic::UnknownHarness { value } if value == "unknown")
        );
    }

    #[test]
    fn parses_effort_all_values() {
        for (s, expected) in [
            ("low", EffortLevel::Low),
            ("medium", EffortLevel::Medium),
            ("high", EffortLevel::High),
            ("xhigh", EffortLevel::XHigh),
        ] {
            let content = format!("---\neffort: {s}\n---\n");
            let (p, diags) = parse(&content);
            assert!(
                diags.is_empty(),
                "unexpected diags for effort={s}: {diags:?}"
            );
            assert_eq!(p.effort, Some(expected));
        }
    }

    #[test]
    fn parses_approval_all_values() {
        for s in ["default", "auto", "confirm", "yolo"] {
            let content = format!("---\napproval: {s}\n---\n");
            let (p, diags) = parse(&content);
            assert!(diags.is_empty(), "unexpected diags for approval={s}");
            assert!(p.approval.is_some());
        }
    }

    #[test]
    fn parses_sandbox_all_values() {
        for s in [
            "default",
            "read-only",
            "workspace-write",
            "danger-full-access",
        ] {
            let content = format!("---\nsandbox: {s}\n---\n");
            let (p, diags) = parse(&content);
            assert!(diags.is_empty(), "unexpected diags for sandbox={s}");
            assert!(p.sandbox.is_some());
        }
    }

    #[test]
    fn parses_autocompact() {
        let (p, diags) = parse("---\nautocompact: 50\n---\n");
        assert!(diags.is_empty());
        assert_eq!(p.autocompact, Some(50));
    }

    #[test]
    fn parses_skills_tools_disallowed_mcp() {
        let content = "---\nskills: [review, dev-principles]\ntools: [Bash, Write]\ndisallowed-tools: [Agent]\nmcp-tools: [server]\n---\n";
        let (p, diags) = parse(content);
        assert!(diags.is_empty());
        assert_eq!(p.skills, vec!["review", "dev-principles"]);
        assert_eq!(p.tools, vec!["Bash", "Write"]);
        assert_eq!(p.disallowed_tools, vec!["Agent"]);
        assert_eq!(p.mcp_tools, vec!["server"]);
    }

    // --- 3.1: model-policies ---

    #[test]
    fn model_policies_are_parsed_as_raw_entries() {
        let content = "---\nmodel-policies:\n  - match:\n      model: gpt-5.5\n    override:\n      harness: codex\n---\n";
        let (p, diags) = parse(content);
        assert!(diags.is_empty());
        assert_eq!(p.model_policies.len(), 1);
    }

    // --- 3.1: fanout ---

    #[test]
    fn fanout_entries_are_parsed_as_raw() {
        let content = "---\nfanout:\n  - alias: opus\n  - model: gpt-5.5\n---\n";
        let (p, diags) = parse(content);
        assert!(diags.is_empty());
        assert_eq!(p.fanout.len(), 2);
    }

    // --- 3.1: harness-overrides ---

    #[test]
    fn harness_overrides_parsed_for_claude_and_codex() {
        let content = "---\nharness-overrides:\n  claude:\n    approval: auto\n  codex:\n    sandbox: workspace-write\n    effort: high\n---\n";
        let (p, diags) = parse(content);
        assert!(diags.is_empty());
        let claude = p.harness_overrides.claude.as_ref().unwrap();
        assert_eq!(claude.approval, Some(ApprovalMode::Auto));
        let codex = p.harness_overrides.codex.as_ref().unwrap();
        assert_eq!(codex.sandbox, Some(SandboxMode::WorkspaceWrite));
        assert_eq!(codex.effort, Some(EffortLevel::High));
    }

    #[test]
    fn harness_override_with_non_overridable_field_produces_diagnostic() {
        let content = "---\nharness-overrides:\n  claude:\n    name: bad\n---\n";
        let (_p, diags) = parse(content);
        assert_eq!(diags.len(), 1);
        assert!(
            matches!(&diags[0], AgentDiagnostic::NonOverridableFieldInOverride { field, .. } if field == "name")
        );
    }

    // --- 3.1: legacy models field ---

    #[test]
    fn legacy_models_field_produces_deprecation_warning() {
        let content = "---\nmodels:\n  opus:\n    effort: high\n---\n";
        let (_p, diags) = parse(content);
        assert_eq!(diags.len(), 1);
        assert!(matches!(&diags[0], AgentDiagnostic::LegacyModelsField));
    }

    // --- Empty agent ---

    #[test]
    fn empty_agent_has_no_diagnostics() {
        let (p, diags) = parse("# Minimal agent\nno frontmatter");
        assert!(diags.is_empty());
        assert!(p.name.is_none());
        assert!(p.harness.is_none());
    }

    #[test]
    fn agent_without_harness_is_universal() {
        let (p, _) = parse("---\nname: planner\nmodel: gpt55\n---\n# Planner");
        assert!(p.harness.is_none());
    }
}
