/// Translation stage — sits between plan creation and disk mutation.
///
/// `TranslatedOutput` wraps a planned action with optional pre-computed
/// content. When `translated_content` is `Some`, the apply stage writes
/// that content instead of re-reading the source path. When `None`, apply
/// falls through to the source path as before.
///
/// Currently all items pass through without transformation — this module
/// exists as the insertion point for future agent/config format lowering
/// (e.g., Claude-native frontmatter stripping, Codex AGENTS.md generation).
use crate::sync::plan::{PlannedAction, SyncPlan};

/// A planned action annotated with optional pre-translated content.
///
/// The `translated_content` field is the seam for per-target format lowering.
/// Populate it in a target-specific translation pass; leave it `None` for
/// pass-through (raw source content).
#[derive(Debug, Clone)]
pub struct TranslatedOutput {
    /// The underlying planned action.
    pub action: PlannedAction,
    /// Pre-computed content to write instead of re-reading the source path.
    /// `None` means "use source content as-is".
    pub translated_content: Option<Vec<u8>>,
}

/// Translate a sync plan into translated outputs.
///
/// Currently a pass-through: every action gets `translated_content = None`.
/// Future: dispatch to a per-target adapter to lower agent/config formats
/// before the apply stage touches the disk.
pub fn translate(plan: &SyncPlan) -> Vec<TranslatedOutput> {
    plan.actions
        .iter()
        .map(|action| TranslatedOutput {
            action: action.clone(),
            translated_content: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::{ItemId, ItemKind};
    use crate::sync::plan::{PlannedAction, SyncPlan};
    use crate::types::DestPath;

    fn make_skip_action() -> PlannedAction {
        PlannedAction::Skip {
            item_id: ItemId {
                kind: ItemKind::Agent,
                name: "coder".into(),
            },
            dest_path: DestPath::from("agents/coder.md"),
            source_name: "base".into(),
            installed_checksum: None,
            reason: "unchanged",
        }
    }

    #[test]
    fn translate_pass_through_preserves_action_count() {
        let plan = SyncPlan {
            actions: vec![make_skip_action(), make_skip_action()],
        };
        let outputs = translate(&plan);
        assert_eq!(outputs.len(), 2);
    }

    #[test]
    fn translate_pass_through_has_no_translated_content() {
        let plan = SyncPlan {
            actions: vec![make_skip_action()],
        };
        let outputs = translate(&plan);
        assert!(outputs[0].translated_content.is_none());
    }

    #[test]
    fn translate_empty_plan_returns_empty() {
        let plan = SyncPlan { actions: vec![] };
        let outputs = translate(&plan);
        assert!(outputs.is_empty());
    }
}
