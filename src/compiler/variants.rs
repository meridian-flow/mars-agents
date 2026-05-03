//! Skill variant layout indexing and projection helpers.
//!
//! Variants are internal to a skill tree. They do not create independent items;
//! this module only validates the `variants/` layout and exposes the harness
//! keys available for native skill projection and CLI annotation.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::diagnostic::DiagnosticCollector;
use crate::error::MarsError;

/// Harness identifiers accepted under `skills/<name>/variants/`.
pub const KNOWN_HARNESS_VARIANT_KEYS: &[&str] = &["claude", "codex", "opencode", "pi", "cursor"];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillVariantIndex {
    harnesses: BTreeMap<String, HarnessVariantIndex>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HarnessVariantIndex {
    has_harness_skill: bool,
    model_keys: Vec<String>,
}

impl SkillVariantIndex {
    pub fn is_empty(&self) -> bool {
        self.harnesses.is_empty()
    }

    pub fn harness_keys(&self) -> impl Iterator<Item = &str> {
        self.harnesses.keys().map(String::as_str)
    }

    #[cfg(test)]
    fn has_harness_skill(&self, harness_key: &str) -> bool {
        self.harnesses
            .get(harness_key)
            .map(|harness| harness.has_harness_skill)
            .unwrap_or(false)
    }

    pub fn annotation(&self) -> Option<String> {
        if self.is_empty() {
            None
        } else {
            Some(self.harness_keys().collect::<Vec<_>>().join(", "))
        }
    }
}

/// Index a skill's `variants/` tree and emit non-fatal layout diagnostics.
pub fn validate_skill_variants(
    skill_dir: &Path,
    skill_name: &str,
    diag: &mut DiagnosticCollector,
) -> SkillVariantIndex {
    let (index, warnings) = index_skill_variants(skill_dir);
    for warning in warnings {
        diag.warn(
            warning.code,
            format!("skill `{skill_name}`: {}", warning.message),
        );
    }
    index
}

/// Index a skill's `variants/` tree without emitting diagnostics.
pub fn index_skill_variants(skill_dir: &Path) -> (SkillVariantIndex, Vec<VariantLayoutWarning>) {
    let variants_dir = skill_dir.join("variants");
    if !variants_dir.is_dir() {
        return (SkillVariantIndex::default(), Vec::new());
    }

    let mut index = SkillVariantIndex::default();
    let mut warnings = Vec::new();
    let Ok(entries) = std::fs::read_dir(&variants_dir) else {
        warnings.push(VariantLayoutWarning::new(
            "skill-variants-read",
            format!("could not read {}", variants_dir.display()),
        ));
        return (index, warnings);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            warnings.push(VariantLayoutWarning::new(
                "skill-variant-name",
                format!("variant path is not valid UTF-8: {}", path.display()),
            ));
            continue;
        };

        let Ok(file_type) = entry.file_type() else {
            warnings.push(VariantLayoutWarning::new(
                "skill-variant-read",
                format!("could not inspect {}", path.display()),
            ));
            continue;
        };

        if !file_type.is_dir() {
            warnings.push(VariantLayoutWarning::new(
                "skill-variant-layout",
                format!("ignoring non-directory entry under variants/: {name}"),
            ));
            continue;
        }

        if !is_known_harness_variant_key(&name) {
            warnings.push(VariantLayoutWarning::new(
                "skill-variant-unknown-harness",
                format!("unknown harness variant `{name}` under variants/"),
            ));
        }

        let harness_index = index_harness_variant(&path, &name, &mut warnings);
        index.harnesses.insert(name, harness_index);
    }

    (index, warnings)
}

fn index_harness_variant(
    harness_dir: &Path,
    harness_key: &str,
    warnings: &mut Vec<VariantLayoutWarning>,
) -> HarnessVariantIndex {
    let mut index = HarnessVariantIndex {
        has_harness_skill: harness_dir.join("SKILL.md").is_file(),
        model_keys: Vec::new(),
    };

    let Ok(entries) = std::fs::read_dir(harness_dir) else {
        warnings.push(VariantLayoutWarning::new(
            "skill-variant-read",
            format!("could not read {}", harness_dir.display()),
        ));
        return index;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            warnings.push(VariantLayoutWarning::new(
                "skill-variant-name",
                format!("variant path is not valid UTF-8: {}", path.display()),
            ));
            continue;
        };
        if name == "SKILL.md" {
            continue;
        }

        let Ok(file_type) = entry.file_type() else {
            warnings.push(VariantLayoutWarning::new(
                "skill-variant-read",
                format!("could not inspect {}", path.display()),
            ));
            continue;
        };

        if !file_type.is_dir() {
            warnings.push(VariantLayoutWarning::new(
                "skill-variant-layout",
                format!("ignoring non-directory entry under variants/{harness_key}/: {name}"),
            ));
            continue;
        }

        if path.join("SKILL.md").is_file() {
            index.model_keys.push(name);
        } else {
            warnings.push(VariantLayoutWarning::new(
                "skill-variant-missing-skill",
                format!("model variant variants/{harness_key}/{name}/ missing SKILL.md"),
            ));
        }
    }

    index.model_keys.sort();
    index
}

fn is_known_harness_variant_key(key: &str) -> bool {
    KNOWN_HARNESS_VARIANT_KEYS.contains(&key)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantLayoutWarning {
    pub code: &'static str,
    pub message: String,
}

impl VariantLayoutWarning {
    fn new(code: &'static str, message: String) -> Self {
        Self { code, message }
    }
}

/// Path to a harness-level replacement document, when one exists.
pub fn harness_skill_variant_path(skill_dir: &Path, harness_key: &str) -> Option<PathBuf> {
    let path = skill_dir
        .join("variants")
        .join(harness_key)
        .join("SKILL.md");
    path.is_file().then_some(path)
}

/// Atomically project one canonical skill tree to a target skill directory.
///
/// Native harness projections copy the whole skill tree except the top-level
/// `variants/` subtree, then optionally replace only `SKILL.md` with the
/// harness-level variant. Passing `None` keeps the full tree and is intended
/// for full-fidelity targets such as `.agents`.
pub fn project_skill_for_target(
    source: &Path,
    dest: &Path,
    harness_variant_key: Option<&str>,
    diag: &mut crate::diagnostic::DiagnosticCollector,
    skill_name: &str,
) -> Result<(), MarsError> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;

    let tmp_dir = tempfile::TempDir::new_in(parent)?;
    if harness_variant_key.is_some() {
        copy_dir_following_symlinks_excluding_top_level_variants(source, tmp_dir.path())?;
    } else {
        copy_dir_following_symlinks(source, tmp_dir.path())?;
    }

    if let Some(variant_path) =
        harness_variant_key.and_then(|key| harness_skill_variant_path(source, key))
    {
        let projected_skill = tmp_dir.path().join("SKILL.md");
        let content = fs::read(variant_path)?;
        fs::write(projected_skill, content)?;
    }

    if let Some(key) = harness_variant_key {
        compile_projected_skill_frontmatter(source, tmp_dir.path(), key, diag, skill_name)?;
    }

    let tmp_path = tmp_dir.keep();
    crate::platform::fs::replace_generated_dir(&tmp_path, dest)
}

fn compile_projected_skill_frontmatter(
    source: &Path,
    projected_dir: &Path,
    harness_key: &str,
    diag: &mut crate::diagnostic::DiagnosticCollector,
    skill_name: &str,
) -> Result<(), MarsError> {
    use crate::compiler::agents::lower::Lossiness;
    use crate::compiler::skills::lower::{SkillHarness, lower_skill_for_harness};
    use crate::compiler::skills::parse_skill_content;

    let Some(harness) = SkillHarness::from_variant_key(harness_key) else {
        return Ok(());
    };

    let base_skill = source.join("SKILL.md");
    let projected_skill = projected_dir.join("SKILL.md");
    let base_content = fs::read_to_string(&base_skill)?;
    let selected_content = fs::read_to_string(&projected_skill)?;

    let mut skill_diags = Vec::new();
    let (profile, _) = match parse_skill_content(&base_content, &mut skill_diags) {
        Ok(parsed) => parsed,
        Err(_) => {
            for d in &skill_diags {
                diag.error_with_category(
                    "skill-schema-error",
                    format!("skill `{skill_name}`: {}", d.message()),
                    crate::diagnostic::DiagnosticCategory::Validation,
                );
            }
            return Ok(());
        }
    };

    for d in &skill_diags {
        if d.is_error() {
            diag.error_with_category(
                "skill-schema-error",
                format!("skill `{skill_name}`: {}", d.message()),
                crate::diagnostic::DiagnosticCategory::Validation,
            );
        } else {
            diag.warn(
                "skill-schema-warning",
                format!("skill `{skill_name}`: {}", d.message()),
            );
        }
    }

    if !profile.has_frontmatter {
        return Ok(());
    }

    let selected_fm = match crate::frontmatter::parse(&selected_content) {
        Ok(fm) => fm,
        Err(e) => {
            diag.error_with_category(
                "skill-schema-error",
                format!("skill `{skill_name}` selected variant frontmatter is malformed; raw fallback used: {e}"),
                crate::diagnostic::DiagnosticCategory::Validation,
            );
            return Ok(());
        }
    };
    let lowered = lower_skill_for_harness(harness, &profile, selected_fm.body());

    for lf in &lowered.lossy_fields {
        match &lf.classification {
            // Dropped/MeridianOnly fields are expected target-format gaps — not actionable.
            Lossiness::Dropped | Lossiness::MeridianOnly => {}
            Lossiness::Approximate { note } => diag.warn(
                "skill-field-approximate",
                format!(
                    "skill `{skill_name}`: field `{}` approximately mapped in {} ({note})",
                    lf.field, lf.target
                ),
            ),
        }
    }

    fs::write(projected_skill, lowered.bytes)?;
    Ok(())
}

fn copy_dir_following_symlinks_excluding_top_level_variants(
    source: &Path,
    dest: &Path,
) -> Result<(), MarsError> {
    fs::create_dir_all(dest)?;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_name() == "variants" {
            continue;
        }
        copy_entry_following_symlinks(&entry.path(), &dest.join(entry.file_name()))?;
    }

    Ok(())
}

fn copy_dir_following_symlinks(source: &Path, dest: &Path) -> Result<(), MarsError> {
    fs::create_dir_all(dest)?;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        copy_entry_following_symlinks(&entry.path(), &dest.join(entry.file_name()))?;
    }

    Ok(())
}

fn copy_entry_following_symlinks(source: &Path, dest: &Path) -> Result<(), MarsError> {
    let metadata = match fs::metadata(source) {
        Ok(m) => m,
        Err(e) => {
            if source.symlink_metadata()?.file_type().is_symlink() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("broken symlink in source tree: {}", source.display()),
                )
                .into());
            }
            return Err(e.into());
        }
    };

    if metadata.is_dir() {
        copy_dir_following_symlinks(source, dest)
    } else if metadata.is_file() {
        let content = fs::read(source)?;
        fs::write(dest, content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dest, fs::Permissions::from_mode(0o644))?;
        }
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unsupported filesystem entry: {}", source.display()),
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::DiagnosticCollector;
    use tempfile::TempDir;

    #[test]
    fn indexes_harness_and_model_variants() {
        let tmp = TempDir::new().unwrap();
        let skill = tmp.path();
        std::fs::create_dir_all(skill.join("variants/claude/opus")).unwrap();
        std::fs::write(skill.join("variants/claude/SKILL.md"), "claude").unwrap();
        std::fs::write(skill.join("variants/claude/opus/SKILL.md"), "opus").unwrap();

        let (index, warnings) = index_skill_variants(skill);

        assert!(warnings.is_empty());
        assert!(index.has_harness_skill("claude"));
        assert_eq!(index.annotation().as_deref(), Some("claude"));
    }

    #[test]
    fn unknown_harness_warns_but_indexes() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("variants/future")).unwrap();
        std::fs::write(tmp.path().join("variants/future/SKILL.md"), "future").unwrap();

        let (index, warnings) = index_skill_variants(tmp.path());

        assert!(index.has_harness_skill("future"));
        assert!(
            warnings
                .iter()
                .any(|w| w.code == "skill-variant-unknown-harness")
        );
    }

    #[test]
    fn model_variant_without_skill_warns() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("variants/codex/gpt55")).unwrap();

        let (_index, warnings) = index_skill_variants(tmp.path());

        assert!(
            warnings
                .iter()
                .any(|w| w.code == "skill-variant-missing-skill")
        );
    }

    #[test]
    fn projects_native_skill_without_variants_and_replaces_skill_md() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(source.join("resources")).unwrap();
        std::fs::create_dir_all(source.join("variants/claude/opus")).unwrap();
        std::fs::write(source.join("SKILL.md"), "base").unwrap();
        std::fs::write(source.join("resources/BOOTSTRAP.md"), "bootstrap").unwrap();
        std::fs::write(source.join("variants/claude/SKILL.md"), "claude").unwrap();
        std::fs::write(source.join("variants/claude/opus/SKILL.md"), "opus").unwrap();

        let mut diag = DiagnosticCollector::new();
        project_skill_for_target(&source, &dest, Some("claude"), &mut diag, "planning").unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("SKILL.md")).unwrap(),
            "claude"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("resources/BOOTSTRAP.md")).unwrap(),
            "bootstrap"
        );
        assert!(!dest.join("variants").exists());
    }

    #[test]
    fn native_projection_lowers_base_frontmatter_with_variant_body() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(source.join("variants/codex")).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: planning\ndescription: Base desc\nmodel-invocable: false\nallowed-tools: [Bash(git *)]\n---\nBase body\n",
        )
        .unwrap();
        std::fs::write(
            source.join("variants/codex/SKILL.md"),
            "---\nname: ignored\n---\nCodex body\n",
        )
        .unwrap();

        let mut diag = DiagnosticCollector::new();
        project_skill_for_target(&source, &dest, Some("codex"), &mut diag, "planning").unwrap();

        let out = std::fs::read_to_string(dest.join("SKILL.md")).unwrap();
        assert!(out.contains("name: planning"));
        assert!(out.contains("allow_implicit_invocation: false"));
        assert!(!out.contains("name: ignored"));
        assert!(!out.contains("allowed-tools"));
        assert!(out.ends_with("Codex body\n"));
        // Dropped fields are silently suppressed (not actionable), so no diagnostics expected.
        assert!(diag.drain().is_empty());
    }

    #[test]
    fn full_fidelity_projection_keeps_variants() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(source.join("variants/codex")).unwrap();
        std::fs::write(source.join("SKILL.md"), "base").unwrap();
        std::fs::write(source.join("variants/codex/SKILL.md"), "codex").unwrap();

        let mut diag = DiagnosticCollector::new();
        project_skill_for_target(&source, &dest, None, &mut diag, "planning").unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("SKILL.md")).unwrap(),
            "base"
        );
        assert!(dest.join("variants/codex/SKILL.md").exists());
    }
}
