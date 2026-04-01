use crate::error::MarsError;
use crate::lock::ItemId;
use crate::sync::target::TargetState;

/// Warning from dependency validation.
///
/// Agents declare `skills: [X, Y]` in YAML frontmatter. After resolution,
/// every referenced skill must exist somewhere in the target state.
#[derive(Debug, Clone)]
pub enum ValidationWarning {
    /// An agent references a skill that doesn't exist in target state.
    MissingSkill {
        agent: ItemId,
        skill_name: String,
        /// Fuzzy match suggestion: "did you mean X?"
        suggestion: Option<String>,
    },
    /// A skill is installed but no agent references it.
    OrphanedSkill { skill: ItemId },
}

/// Check that agent→skill references resolve in the target state.
///
/// Uses `serde_yaml` to parse agent frontmatter. Only reads the YAML
/// front matter block (between `---` delimiters), not the full markdown body.
pub fn check_deps(target: &TargetState) -> Result<Vec<ValidationWarning>, MarsError> {
    let _ = target;
    todo!()
}
