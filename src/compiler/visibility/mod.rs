/// Visibility propagation rules — D1/D10.
///
/// Passive items (Agent, Skill, BootstrapDoc) are **exported** by default and
/// may traverse multiple package boundaries.
///
/// Effectful items (Hook, McpServer) are **local** by default. They only cross
/// package boundaries when the author explicitly marks them `exported: true` in
/// frontmatter, and a diagnostic warning is emitted when they do.
use crate::lock::ItemKind;

/// Visibility class: whether an item is visible outside its source package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityClass {
    /// Item crosses package boundaries by default or explicit opt-in.
    Exported,
    /// Item stays within its source package unless explicitly exported.
    Local,
}

/// Resolved visibility for a single item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemVisibility {
    pub class: VisibilityClass,
    /// Whether this was an explicit `exported: true` override rather than default.
    pub is_explicit_override: bool,
    /// Warning to emit when an effectful item is explicitly exported.
    pub warning: Option<VisibilityWarning>,
}

/// Warning emitted when effectful items cross package boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibilityWarning {
    /// A Hook or McpServer was explicitly marked `exported: true`.
    /// This crosses a package boundary which has side-effect implications.
    EffectfulItemExported {
        kind: ItemKind,
        name: String,
    },
}

/// Determine the default visibility class for an item kind (D1/D10).
///
/// Passive items (agents, skills, bootstrap docs) are exported by default.
/// Effectful items (hooks, MCP servers) are local by default.
pub fn default_visibility(kind: ItemKind) -> VisibilityClass {
    match kind {
        ItemKind::Agent | ItemKind::Skill | ItemKind::BootstrapDoc => VisibilityClass::Exported,
        ItemKind::Hook | ItemKind::McpServer => VisibilityClass::Local,
    }
}

/// Resolve the effective visibility for an item.
///
/// `explicit_exported` is the parsed value of the `exported:` frontmatter field,
/// if present. `None` means the field was absent.
pub fn resolve_visibility(
    kind: ItemKind,
    name: &str,
    explicit_exported: Option<bool>,
) -> ItemVisibility {
    let default = default_visibility(kind);

    match explicit_exported {
        None => ItemVisibility {
            class: default,
            is_explicit_override: false,
            warning: None,
        },
        Some(true) => {
            let warning = if default == VisibilityClass::Local {
                // Effectful item explicitly exported — emit a warning.
                Some(VisibilityWarning::EffectfulItemExported {
                    kind,
                    name: name.to_owned(),
                })
            } else {
                None
            };
            ItemVisibility {
                class: VisibilityClass::Exported,
                is_explicit_override: default == VisibilityClass::Local,
                warning,
            }
        }
        Some(false) => {
            // Explicit local override.
            ItemVisibility {
                class: VisibilityClass::Local,
                is_explicit_override: default == VisibilityClass::Exported,
                warning: None,
            }
        }
    }
}

/// Whether an item can reach a dependent package given its visibility class.
///
/// An item with `VisibilityClass::Exported` can traverse any number of package
/// hops. An item with `VisibilityClass::Local` is invisible to other packages.
pub fn can_cross_package_boundary(visibility: &ItemVisibility) -> bool {
    visibility.class == VisibilityClass::Exported
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::ItemKind;

    #[test]
    fn agents_are_exported_by_default() {
        assert_eq!(default_visibility(ItemKind::Agent), VisibilityClass::Exported);
    }

    #[test]
    fn skills_are_exported_by_default() {
        assert_eq!(default_visibility(ItemKind::Skill), VisibilityClass::Exported);
    }

    #[test]
    fn bootstrap_docs_are_exported_by_default() {
        assert_eq!(
            default_visibility(ItemKind::BootstrapDoc),
            VisibilityClass::Exported
        );
    }

    #[test]
    fn hooks_are_local_by_default() {
        assert_eq!(default_visibility(ItemKind::Hook), VisibilityClass::Local);
    }

    #[test]
    fn mcp_servers_are_local_by_default() {
        assert_eq!(
            default_visibility(ItemKind::McpServer),
            VisibilityClass::Local
        );
    }

    #[test]
    fn resolve_agent_with_no_override_is_exported() {
        let v = resolve_visibility(ItemKind::Agent, "coder", None);
        assert_eq!(v.class, VisibilityClass::Exported);
        assert!(!v.is_explicit_override);
        assert!(v.warning.is_none());
    }

    #[test]
    fn resolve_hook_with_no_override_is_local() {
        let v = resolve_visibility(ItemKind::Hook, "pre-commit", None);
        assert_eq!(v.class, VisibilityClass::Local);
        assert!(!v.is_explicit_override);
        assert!(v.warning.is_none());
    }

    #[test]
    fn resolve_hook_exported_true_emits_warning() {
        let v = resolve_visibility(ItemKind::Hook, "pre-commit", Some(true));
        assert_eq!(v.class, VisibilityClass::Exported);
        assert!(v.is_explicit_override);
        assert!(matches!(
            v.warning,
            Some(VisibilityWarning::EffectfulItemExported {
                kind: ItemKind::Hook,
                ..
            })
        ));
    }

    #[test]
    fn resolve_mcp_exported_true_emits_warning() {
        let v = resolve_visibility(ItemKind::McpServer, "my-server", Some(true));
        assert_eq!(v.class, VisibilityClass::Exported);
        assert!(v.is_explicit_override);
        assert!(v.warning.is_some());
    }

    #[test]
    fn resolve_skill_exported_false_is_local_with_override() {
        let v = resolve_visibility(ItemKind::Skill, "planning", Some(false));
        assert_eq!(v.class, VisibilityClass::Local);
        assert!(v.is_explicit_override);
        assert!(v.warning.is_none());
    }

    #[test]
    fn can_cross_boundary_is_true_for_exported() {
        let v = resolve_visibility(ItemKind::Agent, "coder", None);
        assert!(can_cross_package_boundary(&v));
    }

    #[test]
    fn can_cross_boundary_is_false_for_local() {
        let v = resolve_visibility(ItemKind::Hook, "pre-commit", None);
        assert!(!can_cross_package_boundary(&v));
    }

    #[test]
    fn multi_hop_exported_agent_remains_visible() {
        // Simulate 3-hop dependency chain: A -> B -> C -> agent
        let v = resolve_visibility(ItemKind::Agent, "deep-agent", None);
        // Passive exported items pass through any number of hops.
        assert!(can_cross_package_boundary(&v));
        // And repeating the check doesn't change anything.
        assert!(can_cross_package_boundary(&v));
    }
}
