/// Skill placement parsing and output planning — D33.
///
/// Skills declare an explicit `placement` in frontmatter:
/// - `shared` (default) → emits to `.agents/skills/<name>/`
/// - `routed` → emits only to explicitly named target roots
///
/// A skill-routing prepass extracts `placement` and `targets` as literals
/// before full frontmatter parsing, so routing decisions and overlap checks
/// can run before compilation.
pub mod validate;

use std::collections::HashSet;

use crate::compiler::skills::validate::{
    ISOLATED_TARGETS, OVERLAPPING_TARGETS, SkillPlacementError,
};

/// Placement mode for a skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementMode {
    /// Default: emit to `.agents/skills/<name>/`. May also emit to isolated
    /// consumer roots (e.g. `.claude`) but never to overlapping consumer roots.
    Shared,
    /// Emit only to the explicitly named target roots. `.agents` is forbidden.
    Routed,
}

impl Default for PlacementMode {
    fn default() -> Self {
        Self::Shared
    }
}

/// Resolved placement for a single skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPlacement {
    /// The placement mode (shared or routed).
    pub mode: PlacementMode,
    /// Effective emit set — the target roots this skill will be written to.
    /// Always normalized (forward-slash, lowercase, leading dot preserved).
    pub targets: Vec<String>,
}

impl SkillPlacement {
    /// The `.agents` root — the shared portable skill layer.
    pub const AGENTS_ROOT: &'static str = ".agents";

    /// Construct shared placement with only `.agents`.
    pub fn shared_default() -> Self {
        Self {
            mode: PlacementMode::Shared,
            targets: vec![Self::AGENTS_ROOT.to_owned()],
        }
    }

    /// Resolve `placement` and `targets` from raw frontmatter text (prepass).
    ///
    /// This runs **before** full YAML parsing so routing is available for
    /// overlap detection. Both fields must be literals; non-literal syntax
    /// (anchors, aliases, complex mappings) is a compile-time error.
    ///
    /// `name` is the skill name (used in error messages).
    pub fn from_frontmatter(content: &str, name: &str) -> Result<Self, SkillPlacementError> {
        let raw = extract_raw_placement_block(content);
        let placement_str = raw.placement.as_deref().unwrap_or("shared");
        let mode = parse_placement_mode(placement_str, name)?;

        match mode {
            PlacementMode::Shared => {
                let targets = match raw.targets {
                    None => vec![Self::AGENTS_ROOT.to_owned()],
                    Some(ref ts) => {
                        let targets = parse_literal_targets(ts, name)?;
                        // shared placement: must include .agents
                        if !targets.contains(&Self::AGENTS_ROOT.to_owned()) {
                            return Err(SkillPlacementError::SharedMissingAgents {
                                name: name.to_owned(),
                            });
                        }
                        // shared placement: must not include overlapping consumer roots
                        for target in &targets {
                            if OVERLAPPING_TARGETS.contains(&target.as_str()) {
                                return Err(SkillPlacementError::SharedWithOverlapping {
                                    name: name.to_owned(),
                                    target: target.clone(),
                                });
                            }
                        }
                        targets
                    }
                };
                Ok(Self {
                    mode: PlacementMode::Shared,
                    targets,
                })
            }
            PlacementMode::Routed => {
                let targets = match raw.targets {
                    None => {
                        return Err(SkillPlacementError::RoutedMissingTargets {
                            name: name.to_owned(),
                        });
                    }
                    Some(ref ts) => parse_literal_targets(ts, name)?,
                };
                // routed: .agents is forbidden
                if targets.contains(&Self::AGENTS_ROOT.to_owned()) {
                    return Err(SkillPlacementError::RoutedIncludesAgents {
                        name: name.to_owned(),
                    });
                }
                if targets.is_empty() {
                    return Err(SkillPlacementError::RoutedMissingTargets {
                        name: name.to_owned(),
                    });
                }
                Ok(Self {
                    mode: PlacementMode::Routed,
                    targets,
                })
            }
        }
    }

    /// The output path for this skill under a given target root.
    ///
    /// Returns `"skills/<name>"` — the conventional skill directory layout.
    pub fn output_path_in_target(&self, target: &str, skill_name: &str) -> String {
        let _ = target; // target is implicit in the root dir, not in path
        format!("skills/{skill_name}")
    }

    /// All effective emit paths across all target roots.
    ///
    /// Returns `(target_root, output_path)` pairs, e.g.:
    /// - `(".agents", "skills/planning")`
    /// - `(".codex", "skills/planning")`
    pub fn emit_paths(&self, skill_name: &str) -> Vec<(String, String)> {
        self.targets
            .iter()
            .map(|t| (t.clone(), self.output_path_in_target(t, skill_name)))
            .collect()
    }

    /// Whether this skill is visible to a given consumer target root.
    ///
    /// Uses the consumer visibility model from the spec:
    /// - Overlapping consumers (Codex, OpenCode, Pi) see their own root + `.agents`.
    /// - Isolated consumers (Claude, Cursor) see only their own root.
    /// - Meridian V0 sees only `.agents`.
    pub fn is_visible_to(&self, consumer_root: &str) -> bool {
        compute_consumer_visibility_set(consumer_root)
            .iter()
            .any(|root| self.targets.contains(root))
    }
}

/// Compute the effective skill visibility set for a given consumer.
///
/// - Overlapping consumers: their own root + `.agents`
/// - Isolated consumers: only their own root
/// - Unknown consumers: only `.agents` (fallback to Meridian V0 behavior)
pub fn compute_consumer_visibility_set(consumer_root: &str) -> Vec<String> {
    if OVERLAPPING_TARGETS.contains(&consumer_root) {
        vec![
            consumer_root.to_owned(),
            SkillPlacement::AGENTS_ROOT.to_owned(),
        ]
    } else if ISOLATED_TARGETS.contains(&consumer_root) {
        vec![consumer_root.to_owned()]
    } else {
        // Meridian V0 / unknown → .agents only
        vec![SkillPlacement::AGENTS_ROOT.to_owned()]
    }
}

// ─── Internal parsing helpers ─────────────────────────────────────────────────

/// Raw strings extracted from a content block's frontmatter prepass.
#[derive(Debug, Default)]
struct RawPlacementBlock {
    placement: Option<String>,
    targets: Option<String>,
}

/// Extract `placement` and `targets` as raw strings from markdown frontmatter.
///
/// Minimal line-by-line parse — does not invoke a full YAML parser so it works
/// before the compiler's full frontmatter stage. Only literal scalar/sequence
/// values are accepted.
fn extract_raw_placement_block(content: &str) -> RawPlacementBlock {
    let mut result = RawPlacementBlock::default();
    let mut in_frontmatter = false;
    let mut delimiters_seen = 0u8;
    let mut collecting_targets = false;
    let mut target_lines: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            delimiters_seen += 1;
            if delimiters_seen == 1 {
                in_frontmatter = true;
            } else {
                // End of frontmatter — flush any pending target collection.
                if collecting_targets && !target_lines.is_empty() {
                    result.targets = Some(target_lines.join(","));
                    target_lines.clear();
                }
                in_frontmatter = false;
            }
            collecting_targets = false;
            continue;
        }
        if !in_frontmatter {
            continue;
        }

        // Check if we're in a multi-line targets sequence
        if collecting_targets {
            if trimmed.starts_with('-') {
                // sequence item: "- .codex"
                let item = trimmed.trim_start_matches('-').trim().to_owned();
                target_lines.push(item);
                continue;
            } else if !trimmed.is_empty() && !trimmed.starts_with(' ') {
                // New key: stop collecting targets
                collecting_targets = false;
                result.targets = Some(target_lines.join(","));
                target_lines.clear();
            }
        }

        if let Some(rest) = trimmed.strip_prefix("placement:") {
            result.placement = Some(rest.trim().to_owned());
        } else if let Some(rest) = trimmed.strip_prefix("targets:") {
            let rest = rest.trim();
            if rest.is_empty() {
                // Multi-line list follows
                collecting_targets = true;
                target_lines.clear();
            } else if rest.starts_with('[') {
                // Inline list: [.codex, .opencode]
                let inner = rest.trim_start_matches('[').trim_end_matches(']');
                result.targets = Some(inner.to_owned());
            } else {
                // Single scalar (unusual but handle gracefully)
                result.targets = Some(rest.to_owned());
            }
        }
    }

    // Flush targets if we hit EOF while collecting
    if collecting_targets && !target_lines.is_empty() {
        result.targets = Some(target_lines.join(","));
    }

    result
}

fn parse_placement_mode(value: &str, name: &str) -> Result<PlacementMode, SkillPlacementError> {
    match value.trim() {
        "shared" => Ok(PlacementMode::Shared),
        "routed" => Ok(PlacementMode::Routed),
        other => Err(SkillPlacementError::InvalidPlacementValue {
            name: name.to_owned(),
            value: other.to_owned(),
        }),
    }
}

fn parse_literal_targets(raw: &str, name: &str) -> Result<Vec<String>, SkillPlacementError> {
    // Accept comma-separated list (from inline or multi-line extraction)
    let targets: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_owned())
        .filter(|s| !s.is_empty())
        .collect();

    for target in &targets {
        if !target.starts_with('.') {
            return Err(SkillPlacementError::InvalidTargetName {
                name: name.to_owned(),
                target: target.clone(),
            });
        }
    }

    // Deduplicate while preserving order
    let mut seen = HashSet::new();
    let deduped = targets
        .into_iter()
        .filter(|t| seen.insert(t.clone()))
        .collect();

    Ok(deduped)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_raw_placement_block ──────────────────────────────────────────

    #[test]
    fn extracts_placement_shared() {
        let content = "---\nname: foo\nplacement: shared\n---\nbody";
        let raw = extract_raw_placement_block(content);
        assert_eq!(raw.placement.as_deref(), Some("shared"));
    }

    #[test]
    fn extracts_placement_routed() {
        let content = "---\nname: foo\nplacement: routed\ntargets: [.codex, .opencode]\n---\nbody";
        let raw = extract_raw_placement_block(content);
        assert_eq!(raw.placement.as_deref(), Some("routed"));
        assert!(raw.targets.is_some());
    }

    #[test]
    fn extracts_multiline_targets() {
        let content = "---\nname: foo\nplacement: routed\ntargets:\n  - .codex\n  - .pi\n---\nbody";
        let raw = extract_raw_placement_block(content);
        let targets = raw.targets.unwrap();
        assert!(targets.contains(".codex"));
        assert!(targets.contains(".pi"));
    }

    #[test]
    fn no_frontmatter_returns_empty() {
        let content = "no frontmatter here";
        let raw = extract_raw_placement_block(content);
        assert!(raw.placement.is_none());
        assert!(raw.targets.is_none());
    }

    // ── SkillPlacement::from_frontmatter ────────────────────────────────────

    #[test]
    fn default_placement_is_shared_agents() {
        let content = "---\nname: my-skill\n---\nbody";
        let p = SkillPlacement::from_frontmatter(content, "my-skill").unwrap();
        assert_eq!(p.mode, PlacementMode::Shared);
        assert_eq!(p.targets, vec![".agents"]);
    }

    #[test]
    fn explicit_shared_without_targets_defaults_to_agents() {
        let content = "---\nplacement: shared\n---\nbody";
        let p = SkillPlacement::from_frontmatter(content, "s").unwrap();
        assert_eq!(p.mode, PlacementMode::Shared);
        assert_eq!(p.targets, vec![".agents"]);
    }

    #[test]
    fn shared_with_agents_and_claude_is_valid() {
        let content = "---\nplacement: shared\ntargets: [.agents, .claude]\n---\nbody";
        let p = SkillPlacement::from_frontmatter(content, "s").unwrap();
        assert_eq!(p.mode, PlacementMode::Shared);
        assert!(p.targets.contains(&".agents".to_owned()));
        assert!(p.targets.contains(&".claude".to_owned()));
    }

    #[test]
    fn shared_with_agents_and_codex_is_error() {
        let content = "---\nplacement: shared\ntargets: [.agents, .codex]\n---\nbody";
        let err = SkillPlacement::from_frontmatter(content, "s").unwrap_err();
        assert!(matches!(
            err,
            SkillPlacementError::SharedWithOverlapping { .. }
        ));
    }

    #[test]
    fn shared_with_agents_and_opencode_is_error() {
        let content = "---\nplacement: shared\ntargets: [.agents, .opencode]\n---\nbody";
        let err = SkillPlacement::from_frontmatter(content, "s").unwrap_err();
        assert!(matches!(
            err,
            SkillPlacementError::SharedWithOverlapping { .. }
        ));
    }

    #[test]
    fn shared_with_agents_and_pi_is_error() {
        let content = "---\nplacement: shared\ntargets: [.agents, .pi]\n---\nbody";
        let err = SkillPlacement::from_frontmatter(content, "s").unwrap_err();
        assert!(matches!(
            err,
            SkillPlacementError::SharedWithOverlapping { .. }
        ));
    }

    #[test]
    fn shared_targets_must_include_agents() {
        let content = "---\nplacement: shared\ntargets: [.claude]\n---\nbody";
        let err = SkillPlacement::from_frontmatter(content, "s").unwrap_err();
        assert!(matches!(
            err,
            SkillPlacementError::SharedMissingAgents { .. }
        ));
    }

    #[test]
    fn routed_without_targets_is_error() {
        let content = "---\nplacement: routed\n---\nbody";
        let err = SkillPlacement::from_frontmatter(content, "s").unwrap_err();
        assert!(matches!(
            err,
            SkillPlacementError::RoutedMissingTargets { .. }
        ));
    }

    #[test]
    fn routed_with_agents_is_error() {
        let content = "---\nplacement: routed\ntargets: [.agents, .codex]\n---\nbody";
        let err = SkillPlacement::from_frontmatter(content, "s").unwrap_err();
        assert!(matches!(
            err,
            SkillPlacementError::RoutedIncludesAgents { .. }
        ));
    }

    #[test]
    fn routed_to_codex_opencode_pi_is_valid() {
        let content = "---\nplacement: routed\ntargets: [.codex, .opencode, .pi]\n---\nbody";
        let p = SkillPlacement::from_frontmatter(content, "s").unwrap();
        assert_eq!(p.mode, PlacementMode::Routed);
        assert!(p.targets.contains(&".codex".to_owned()));
        assert!(p.targets.contains(&".opencode".to_owned()));
        assert!(p.targets.contains(&".pi".to_owned()));
    }

    #[test]
    fn routed_to_claude_and_cursor_is_valid() {
        let content = "---\nplacement: routed\ntargets: [.claude, .cursor]\n---\nbody";
        let p = SkillPlacement::from_frontmatter(content, "s").unwrap();
        assert_eq!(p.mode, PlacementMode::Routed);
        assert!(p.targets.contains(&".claude".to_owned()));
        assert!(p.targets.contains(&".cursor".to_owned()));
    }

    #[test]
    fn invalid_placement_value_is_error() {
        let content = "---\nplacement: hybrid\n---\nbody";
        let err = SkillPlacement::from_frontmatter(content, "s").unwrap_err();
        assert!(matches!(
            err,
            SkillPlacementError::InvalidPlacementValue { .. }
        ));
    }

    // ── emit_paths ──────────────────────────────────────────────────────────

    #[test]
    fn shared_emit_paths_are_agents_only_by_default() {
        let p = SkillPlacement::shared_default();
        let paths = p.emit_paths("planning");
        assert_eq!(
            paths,
            vec![(".agents".to_owned(), "skills/planning".to_owned())]
        );
    }

    #[test]
    fn routed_emit_paths_span_all_targets() {
        let p = SkillPlacement {
            mode: PlacementMode::Routed,
            targets: vec![".codex".to_owned(), ".opencode".to_owned()],
        };
        let paths = p.emit_paths("research");
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&(".codex".to_owned(), "skills/research".to_owned())));
        assert!(paths.contains(&(".opencode".to_owned(), "skills/research".to_owned())));
    }

    // ── is_visible_to ───────────────────────────────────────────────────────

    #[test]
    fn shared_skill_visible_to_codex_via_agents() {
        let p = SkillPlacement::shared_default();
        // Codex visibility set = {.codex, .agents} — shared skill is in .agents
        assert!(p.is_visible_to(".codex"));
    }

    #[test]
    fn shared_skill_not_visible_to_claude_directly() {
        // Claude visibility set = {.claude} — shared skill is only in .agents
        let p = SkillPlacement::shared_default();
        assert!(!p.is_visible_to(".claude"));
    }

    #[test]
    fn shared_agents_and_claude_skill_visible_to_both() {
        let p = SkillPlacement {
            mode: PlacementMode::Shared,
            targets: vec![".agents".to_owned(), ".claude".to_owned()],
        };
        // .agents in Codex visibility set → visible
        assert!(p.is_visible_to(".codex"));
        // .claude in Claude visibility set → visible
        assert!(p.is_visible_to(".claude"));
    }

    #[test]
    fn routed_codex_skill_not_visible_to_meridian_v0() {
        let p = SkillPlacement {
            mode: PlacementMode::Routed,
            targets: vec![".codex".to_owned()],
        };
        // Meridian V0 (no specific target) → .agents only
        assert!(!p.is_visible_to(".agents"));
    }

    #[test]
    fn routed_skill_visible_to_named_target() {
        let p = SkillPlacement {
            mode: PlacementMode::Routed,
            targets: vec![".opencode".to_owned(), ".pi".to_owned()],
        };
        assert!(p.is_visible_to(".opencode"));
        assert!(p.is_visible_to(".pi"));
        assert!(!p.is_visible_to(".codex"));
    }

    // ── compute_consumer_visibility_set ─────────────────────────────────────

    #[test]
    fn codex_visibility_set_includes_agents() {
        let set = compute_consumer_visibility_set(".codex");
        assert!(set.contains(&".codex".to_owned()));
        assert!(set.contains(&".agents".to_owned()));
    }

    #[test]
    fn claude_visibility_set_is_isolated() {
        let set = compute_consumer_visibility_set(".claude");
        assert_eq!(set, vec![".claude".to_owned()]);
    }

    #[test]
    fn cursor_visibility_set_is_isolated() {
        let set = compute_consumer_visibility_set(".cursor");
        assert_eq!(set, vec![".cursor".to_owned()]);
    }

    #[test]
    fn unknown_consumer_falls_back_to_agents() {
        let set = compute_consumer_visibility_set(".unknown");
        assert_eq!(set, vec![".agents".to_owned()]);
    }
}
