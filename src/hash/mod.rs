use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::MarsError;
use crate::types::ItemKind;

/// Compute SHA-256 of a file or directory (for skills).
///
/// For agents (single `.md` file): SHA-256 of file content.
/// For skills (directory): SHA-256 of sorted `(relative_path, file_hash)` pairs —
/// deterministic regardless of filesystem ordering.
///
/// Output format: `"sha256:<64-char-hex>"`.
pub fn compute_hash(path: &Path, kind: ItemKind) -> Result<String, MarsError> {
    match kind {
        ItemKind::Agent | ItemKind::Hook | ItemKind::McpServer | ItemKind::BootstrapDoc => {
            let content = fs::read(path)?;
            Ok(hash_bytes(&content))
        }
        ItemKind::Skill => compute_dir_hash(path),
    }
}

/// Compute hash for a skill directory while excluding selected top-level entries.
pub fn compute_skill_hash_filtered(
    dir: &Path,
    excluded_top_level: &[&str],
) -> Result<String, MarsError> {
    compute_dir_hash_filtered(dir, excluded_top_level)
}

/// Compute SHA-256 of raw bytes.
///
/// Returns `"sha256:<64-char-lowercase-hex>"`.
pub fn hash_bytes(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    let digest = hasher.finalize();
    format!("sha256:{:064x}", digest)
}

/// Compute a deterministic hash for a directory by:
/// 1. Walking all files recursively
/// 2. Collecting (relative_path, file_sha256) pairs
/// 3. Sorting lexicographically by path
/// 4. Concatenating "path:hash\n" strings
/// 5. SHA-256 of the concatenated result
fn compute_dir_hash(dir: &Path) -> Result<String, MarsError> {
    compute_dir_hash_filtered(dir, &[])
}

fn compute_dir_hash_filtered(dir: &Path, excluded_top_level: &[&str]) -> Result<String, MarsError> {
    let mut entries: Vec<(String, String)> = Vec::new();
    collect_file_hashes(dir, dir, &mut entries, excluded_top_level)?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut manifest = String::new();
    for (rel_path, hash) in &entries {
        manifest.push_str(rel_path);
        manifest.push(':');
        manifest.push_str(hash);
        manifest.push('\n');
    }

    Ok(hash_bytes(manifest.as_bytes()))
}

/// Recursively collect (relative_path, hash) pairs for all files in a directory.
fn collect_file_hashes(
    root: &Path,
    current: &Path,
    entries: &mut Vec<(String, String)>,
    excluded_top_level: &[&str],
) -> Result<(), MarsError> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        let rel_path = path.strip_prefix(root).expect("path is always under root");
        if is_excluded_top_level(rel_path, excluded_top_level) {
            continue;
        }

        if file_type.is_dir() {
            collect_file_hashes(root, &path, entries, excluded_top_level)?;
        } else {
            // Build forward-slash relative path from components for cross-platform determinism
            let rel_path: String = rel_path
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let content = fs::read(&path)?;
            let hash = hash_bytes(&content);
            entries.push((rel_path, hash));
        }
    }
    Ok(())
}

fn is_excluded_top_level(path: &Path, excluded_top_level: &[&str]) -> bool {
    let Some(first) = path.components().next().map(|c| c.as_os_str()) else {
        return false;
    };
    excluded_top_level.iter().any(|excluded| first == *excluded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn hash_bytes_returns_lowercase_hex() {
        let hash = hash_bytes(b"test");
        assert!(hash.starts_with("sha256:"));
        let hex = &hash["sha256:".len()..];
        assert_eq!(hex.len(), 64);
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn compute_hash_skill_directory() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("my-skill");
        fs::create_dir_all(skill_dir.join("sub")).unwrap();
        fs::write(skill_dir.join("main.md"), "main content").unwrap();
        fs::write(skill_dir.join("sub").join("helper.md"), "helper content").unwrap();

        let hash = compute_hash(&skill_dir, ItemKind::Skill).unwrap();
        assert!(hash.starts_with("sha256:"));

        // Verify determinism: same content → same hash
        let hash2 = compute_hash(&skill_dir, ItemKind::Skill).unwrap();
        assert_eq!(hash, hash2);
    }

    #[test]
    fn dir_hash_deterministic_regardless_of_creation_order() {
        let dir1 = TempDir::new().unwrap();
        let skill1 = dir1.path().join("skill");
        fs::create_dir_all(&skill1).unwrap();
        // Create files in order: a, b
        fs::write(skill1.join("a.md"), "content a").unwrap();
        fs::write(skill1.join("b.md"), "content b").unwrap();

        let dir2 = TempDir::new().unwrap();
        let skill2 = dir2.path().join("skill");
        fs::create_dir_all(&skill2).unwrap();
        // Create files in reverse order: b, a
        fs::write(skill2.join("b.md"), "content b").unwrap();
        fs::write(skill2.join("a.md"), "content a").unwrap();

        let hash1 = compute_hash(&skill1, ItemKind::Skill).unwrap();
        let hash2 = compute_hash(&skill2, ItemKind::Skill).unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn dir_hash_changes_with_different_content() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("file.md"), "version 1").unwrap();

        let hash1 = compute_hash(&skill_dir, ItemKind::Skill).unwrap();

        fs::write(skill_dir.join("file.md"), "version 2").unwrap();

        let hash2 = compute_hash(&skill_dir, ItemKind::Skill).unwrap();
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn dir_hash_changes_with_different_filename() {
        let dir1 = TempDir::new().unwrap();
        let skill1 = dir1.path().join("skill");
        fs::create_dir_all(&skill1).unwrap();
        fs::write(skill1.join("a.md"), "content").unwrap();

        let dir2 = TempDir::new().unwrap();
        let skill2 = dir2.path().join("skill");
        fs::create_dir_all(&skill2).unwrap();
        fs::write(skill2.join("b.md"), "content").unwrap();

        let hash1 = compute_hash(&skill1, ItemKind::Skill).unwrap();
        let hash2 = compute_hash(&skill2, ItemKind::Skill).unwrap();
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn filtered_skill_hash_ignores_excluded_top_level_entries() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("skill");
        fs::create_dir_all(skill_dir.join(".git")).unwrap();
        fs::write(skill_dir.join("SKILL.md"), "base").unwrap();
        fs::write(skill_dir.join("mars.toml"), "v1").unwrap();
        fs::write(skill_dir.join(".git").join("config"), "ignored").unwrap();

        let hash1 =
            compute_skill_hash_filtered(&skill_dir, crate::fs::FLAT_SKILL_EXCLUDED_TOP_LEVEL)
                .unwrap();

        fs::write(skill_dir.join("mars.toml"), "v2").unwrap();
        fs::write(skill_dir.join(".git").join("config"), "changed").unwrap();

        let hash2 =
            compute_skill_hash_filtered(&skill_dir, crate::fs::FLAT_SKILL_EXCLUDED_TOP_LEVEL)
                .unwrap();

        assert_eq!(hash1, hash2);
    }
}
