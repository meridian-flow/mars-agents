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

    let tmp_path = tmp_dir.keep();
    crate::platform::fs::replace_generated_dir(&tmp_path, dest)
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

        project_skill_for_target(&source, &dest, Some("claude")).unwrap();

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
    fn full_fidelity_projection_keeps_variants() {
        let tmp = TempDir::new().unwrap();
        let source = tmp.path().join("source");
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(source.join("variants/codex")).unwrap();
        std::fs::write(source.join("SKILL.md"), "base").unwrap();
        std::fs::write(source.join("variants/codex/SKILL.md"), "codex").unwrap();

        project_skill_for_target(&source, &dest, None).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("SKILL.md")).unwrap(),
            "base"
        );
        assert!(dest.join("variants/codex/SKILL.md").exists());
    }
}
