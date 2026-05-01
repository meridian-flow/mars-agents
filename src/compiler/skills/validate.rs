/// Compile-time overlap detection and agent→skill visibility validation — D29/D33/D34.
///
/// # Invariant
/// For every logical skill name and every consumer visibility set, the number of
/// visible compiled outputs must be 0 or 1, never greater than 1.
///
/// # Errors
/// - `DuplicateSkillVisibility`: same skill name visible twice to one consumer.
/// - `RoutedSkillNotVisibleToAgent`: agent references a skill it cannot see.
use std::collections::HashMap;

use crate::compiler::skills::{SkillPlacement, compute_consumer_visibility_set};

/// Overlapping consumer targets: they see their own root **plus** `.agents`.
pub const OVERLAPPING_TARGETS: &[&str] = &[".codex", ".opencode", ".pi"];

/// Isolated consumer targets: they see **only** their own root.
pub const ISOLATED_TARGETS: &[&str] = &[".claude", ".cursor"];

/// All known consumer targets (for full overlap sweep).
pub const ALL_CONSUMER_ROOTS: &[&str] = &[
    ".agents",   // Meridian V0
    ".codex",    // overlapping
    ".opencode", // overlapping
    ".pi",       // overlapping
    ".claude",   // isolated
    ".cursor",   // isolated
];

/// A planned skill output: (compiled_skill_name, target_root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedSkillOutput {
    /// The effective skill name after rename-map application.
    pub compiled_name: String,
    /// The target root this output will land in, e.g. `.agents` or `.codex`.
    pub target_root: String,
}

/// An agent's skill reference with its effective execution surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSkillRef {
    /// Agent name (for error messages).
    pub agent_name: String,
    /// Skill name referenced in the agent's frontmatter.
    pub skill_name: String,
    /// The agent's execution target root, if known (e.g. `.codex`).
    /// `None` means universal agent — only sees `.agents`.
    pub agent_target: Option<String>,
}

impl AgentSkillRef {
    /// The effective skill visibility set for this agent.
    pub fn visibility_set(&self) -> Vec<String> {
        match &self.agent_target {
            Some(target) => compute_consumer_visibility_set(target),
            None => vec![SkillPlacement::AGENTS_ROOT.to_owned()],
        }
    }
}

/// Compile-time error for skill placement violations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkillPlacementError {
    /// `placement: shared` combined with an overlapping consumer root.
    #[error(
        "skill `{name}`: shared placement cannot target overlapping consumer `{target}` — \
         use `placement: routed` for target-specific skills"
    )]
    SharedWithOverlapping { name: String, target: String },

    /// `placement: routed` cannot include `.agents`.
    #[error(
        "skill `{name}`: routed placement cannot include `.agents` — \
         routed skills are not allowed in the shared portable layer"
    )]
    RoutedIncludesAgents { name: String },

    /// `placement: routed` must provide a non-empty `targets` list.
    #[error(
        "skill `{name}`: placement is `routed` but no `targets` were declared — \
         routed skills must enumerate their target roots explicitly"
    )]
    RoutedMissingTargets { name: String },

    /// `placement: shared` with explicit targets must include `.agents`.
    #[error(
        "skill `{name}`: shared placement with explicit `targets` must include `.agents`"
    )]
    SharedMissingAgents { name: String },

    /// Unknown `placement:` value.
    #[error(
        "skill `{name}`: unknown placement value `{value}` — expected `shared` or `routed`"
    )]
    InvalidPlacementValue { name: String, value: String },

    /// Target name doesn't start with `.`.
    #[error(
        "skill `{name}`: invalid target name `{target}` — target roots must start with `.`"
    )]
    InvalidTargetName { name: String, target: String },
}

/// Compile-time validation error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkillValidationError {
    /// Two skill outputs are both visible to the same consumer.
    #[error(
        "skill `{skill_name}` is visible to consumer `{consumer}` from multiple roots: {roots:?}"
    )]
    DuplicateSkillVisibility {
        skill_name: String,
        consumer: String,
        roots: Vec<String>,
    },

    /// An agent references a skill with no output in its visibility set.
    #[error(
        "agent `{agent_name}` references skill `{skill_name}` but the skill has no output \
         visible to that agent's execution surface (visibility set: {visibility_set:?}, \
         skill outputs: {skill_outputs:?})"
    )]
    RoutedSkillNotVisibleToAgent {
        agent_name: String,
        skill_name: String,
        visibility_set: Vec<String>,
        skill_outputs: Vec<String>,
    },
}

/// Run compile-time overlap detection over all planned skill outputs.
///
/// Enforces D29: for every `(compiled_skill_name, consumer)` pair, at most
/// one output root may be visible to that consumer.
///
/// The check is cross-package: call with all planned outputs from the full
/// dependency graph.
pub fn check_overlap(
    planned: &[PlannedSkillOutput],
) -> Vec<SkillValidationError> {
    let mut errors = Vec::new();

    // Group by (skill_name, consumer) → list of visible roots
    let mut visibility_counts: HashMap<(String, String), Vec<String>> = HashMap::new();

    for output in planned {
        for consumer in ALL_CONSUMER_ROOTS {
            let consumer_set = compute_consumer_visibility_set(consumer);
            if consumer_set.contains(&output.target_root) {
                let key = (output.compiled_name.clone(), consumer.to_string());
                visibility_counts
                    .entry(key)
                    .or_default()
                    .push(output.target_root.clone());
            }
        }
    }

    for ((skill_name, consumer), roots) in &visibility_counts {
        if roots.len() > 1 {
            errors.push(SkillValidationError::DuplicateSkillVisibility {
                skill_name: skill_name.clone(),
                consumer: consumer.clone(),
                roots: roots.clone(),
            });
        }
    }

    errors
}

/// Validate agent→skill references against the planned skill outputs.
///
/// For each agent skill reference, verify the referenced skill has at least
/// one output visible in the agent's effective skill visibility set.
///
/// Returns an error for each unresolvable reference.
pub fn check_agent_skill_refs(
    agent_refs: &[AgentSkillRef],
    planned: &[PlannedSkillOutput],
) -> Vec<SkillValidationError> {
    let mut errors = Vec::new();

    for agent_ref in agent_refs {
        let visibility_set = agent_ref.visibility_set();

        // Collect all output roots for this skill name
        let skill_outputs: Vec<String> = planned
            .iter()
            .filter(|o| o.compiled_name == agent_ref.skill_name)
            .map(|o| o.target_root.clone())
            .collect();

        // Check if any output is in the agent's visibility set
        let visible = skill_outputs
            .iter()
            .any(|root| visibility_set.contains(root));

        if !visible {
            errors.push(SkillValidationError::RoutedSkillNotVisibleToAgent {
                agent_name: agent_ref.agent_name.clone(),
                skill_name: agent_ref.skill_name.clone(),
                visibility_set,
                skill_outputs,
            });
        }
    }

    errors
}

/// Run all compile-time skill validations and return all errors found.
pub fn validate_all(
    planned: &[PlannedSkillOutput],
    agent_refs: &[AgentSkillRef],
) -> Vec<SkillValidationError> {
    let mut errors = check_overlap(planned);
    errors.extend(check_agent_skill_refs(agent_refs, planned));
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_output(name: &str) -> PlannedSkillOutput {
        PlannedSkillOutput {
            compiled_name: name.to_owned(),
            target_root: ".agents".to_owned(),
        }
    }

    fn target_output(name: &str, target: &str) -> PlannedSkillOutput {
        PlannedSkillOutput {
            compiled_name: name.to_owned(),
            target_root: target.to_owned(),
        }
    }

    fn universal_agent_ref(agent: &str, skill: &str) -> AgentSkillRef {
        AgentSkillRef {
            agent_name: agent.to_owned(),
            skill_name: skill.to_owned(),
            agent_target: None,
        }
    }

    fn targeted_agent_ref(agent: &str, skill: &str, target: &str) -> AgentSkillRef {
        AgentSkillRef {
            agent_name: agent.to_owned(),
            skill_name: skill.to_owned(),
            agent_target: Some(target.to_owned()),
        }
    }

    // ── check_overlap ────────────────────────────────────────────────────────

    #[test]
    fn no_overlap_single_shared_skill() {
        let planned = vec![shared_output("planning")];
        let errors = check_overlap(&planned);
        assert!(errors.is_empty());
    }

    #[test]
    fn no_overlap_routed_to_codex_only() {
        let planned = vec![target_output("repo-research", ".codex")];
        let errors = check_overlap(&planned);
        assert!(errors.is_empty());
    }

    #[test]
    fn no_overlap_agents_plus_claude_for_same_name() {
        // .agents + .claude is valid: Codex sees .agents only; Claude sees .claude only
        let planned = vec![
            shared_output("shell-basics"),
            target_output("shell-basics", ".claude"),
        ];
        let errors = check_overlap(&planned);
        assert!(
            errors.is_empty(),
            "agents + claude must be valid, got: {errors:?}"
        );
    }

    #[test]
    fn overlap_agents_plus_codex_for_same_name_is_error() {
        // Codex visibility = {.codex, .agents} — two roots for "research"
        let planned = vec![
            shared_output("research"),
            target_output("research", ".codex"),
        ];
        let errors = check_overlap(&planned);
        assert!(
            !errors.is_empty(),
            "agents + codex for same skill must be an error"
        );
        assert!(errors.iter().any(|e| matches!(
            e,
            SkillValidationError::DuplicateSkillVisibility { consumer, .. }
            if consumer == ".codex"
        )));
    }

    #[test]
    fn overlap_agents_plus_opencode_for_same_name_is_error() {
        let planned = vec![
            shared_output("research"),
            target_output("research", ".opencode"),
        ];
        let errors = check_overlap(&planned);
        assert!(!errors.is_empty());
    }

    #[test]
    fn overlap_agents_plus_pi_for_same_name_is_error() {
        let planned = vec![
            shared_output("research"),
            target_output("research", ".pi"),
        ];
        let errors = check_overlap(&planned);
        assert!(!errors.is_empty());
    }

    #[test]
    fn no_overlap_different_skill_names() {
        let planned = vec![
            shared_output("planning"),
            target_output("repo-research", ".codex"),
        ];
        let errors = check_overlap(&planned);
        assert!(errors.is_empty());
    }

    #[test]
    fn cross_package_same_name_same_root_is_overlap() {
        // Two packages both emit "planning" to .agents
        let planned = vec![shared_output("planning"), shared_output("planning")];
        let errors = check_overlap(&planned);
        assert!(!errors.is_empty());
    }

    // ── check_agent_skill_refs ───────────────────────────────────────────────

    #[test]
    fn universal_agent_referencing_shared_skill_is_valid() {
        let planned = vec![shared_output("planning")];
        let refs = vec![universal_agent_ref("orchestrator", "planning")];
        let errors = check_agent_skill_refs(&refs, &planned);
        assert!(errors.is_empty());
    }

    #[test]
    fn universal_agent_referencing_routed_codex_skill_is_error() {
        let planned = vec![target_output("repo-research", ".codex")];
        let refs = vec![universal_agent_ref("orchestrator", "repo-research")];
        let errors = check_agent_skill_refs(&refs, &planned);
        assert!(!errors.is_empty());
        assert!(matches!(
            &errors[0],
            SkillValidationError::RoutedSkillNotVisibleToAgent { agent_name, .. }
            if agent_name == "orchestrator"
        ));
    }

    #[test]
    fn codex_agent_referencing_routed_codex_skill_is_valid() {
        let planned = vec![target_output("repo-research", ".codex")];
        let refs = vec![targeted_agent_ref("codex-researcher", "repo-research", ".codex")];
        let errors = check_agent_skill_refs(&refs, &planned);
        assert!(errors.is_empty());
    }

    #[test]
    fn codex_agent_referencing_shared_skill_is_valid() {
        // Codex visibility = {.codex, .agents} — shared skill in .agents is visible
        let planned = vec![shared_output("planning")];
        let refs = vec![targeted_agent_ref("codex-researcher", "planning", ".codex")];
        let errors = check_agent_skill_refs(&refs, &planned);
        assert!(errors.is_empty());
    }

    #[test]
    fn claude_agent_referencing_claude_routed_skill_is_valid() {
        let planned = vec![target_output("shell-basics", ".claude")];
        let refs = vec![targeted_agent_ref("claude-agent", "shell-basics", ".claude")];
        let errors = check_agent_skill_refs(&refs, &planned);
        assert!(errors.is_empty());
    }

    #[test]
    fn claude_agent_referencing_agents_shared_skill_is_invalid() {
        // Claude visibility = {.claude} only — cannot see .agents
        let planned = vec![shared_output("planning")];
        let refs = vec![targeted_agent_ref("claude-agent", "planning", ".claude")];
        let errors = check_agent_skill_refs(&refs, &planned);
        assert!(!errors.is_empty());
    }

    #[test]
    fn missing_skill_altogether_is_error() {
        let planned = vec![];
        let refs = vec![universal_agent_ref("orchestrator", "nonexistent")];
        let errors = check_agent_skill_refs(&refs, &planned);
        assert!(!errors.is_empty());
    }

    // ── validate_all ──────────────────────────────────────────────────────────

    #[test]
    fn validate_all_accumulates_both_error_types() {
        // Overlap: agents + codex for "research"
        let planned = vec![
            shared_output("research"),
            target_output("research", ".codex"),
        ];
        // Missing skill ref: "nonexistent"
        let refs = vec![universal_agent_ref("orchestrator", "nonexistent")];
        let errors = validate_all(&planned, &refs);
        assert!(errors.len() >= 2);
    }

    // ── AgentSkillRef::visibility_set ────────────────────────────────────────

    #[test]
    fn universal_agent_visibility_is_agents_only() {
        let r = universal_agent_ref("o", "s");
        assert_eq!(r.visibility_set(), vec![".agents".to_owned()]);
    }

    #[test]
    fn codex_agent_visibility_includes_agents() {
        let r = targeted_agent_ref("o", "s", ".codex");
        let set = r.visibility_set();
        assert!(set.contains(&".codex".to_owned()));
        assert!(set.contains(&".agents".to_owned()));
    }

    #[test]
    fn claude_agent_visibility_is_only_claude() {
        let r = targeted_agent_ref("o", "s", ".claude");
        assert_eq!(r.visibility_set(), vec![".claude".to_owned()]);
    }

    #[test]
    fn cursor_agent_visibility_is_only_cursor() {
        let r = targeted_agent_ref("o", "s", ".cursor");
        assert_eq!(r.visibility_set(), vec![".cursor".to_owned()]);
    }
}
