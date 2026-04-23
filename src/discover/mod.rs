use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::error::MarsError;
use crate::lock::{ItemId, ItemKind};
use crate::types::ItemName;

const RECURSIVE_SKIP_DIRS: &[&str] = &["node_modules", ".git", "dist", "build", "__pycache__"];
const PLUGIN_MANIFESTS: &[&str] = &[
    ".claude-plugin/plugin.json",
    ".claude-plugin/marketplace.json",
];
const MAX_FALLBACK_DEPTH: usize = 5;
const MAX_CONTAINER_ROOT_DEPTH: usize = 2;
const MAX_HEURISTIC_FS_DEPTH: usize = MAX_FALLBACK_DEPTH + MAX_CONTAINER_ROOT_DEPTH;
const SKILL_CONTAINER_ROOTS: &[&str] = &[
    "skills",
    "skills/.curated",
    "skills/.experimental",
    "skills/.system",
    ".claude/skills",
    ".codex/skills",
    ".agents/skills",
];
const AGENT_CONTAINER_ROOTS: &[&str] = &[
    "agents",
    ".claude/agents",
    ".codex/agents",
    ".agents/agents",
];
const MANIFEST_SKILL_KEYS: &[&str] = &["skills", "skill_paths", "skillPaths"];
const MANIFEST_AGENT_KEYS: &[&str] = &["agents", "agent_paths", "agentPaths"];

/// An item discovered in a source tree by filesystem convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredItem {
    pub id: ItemId,
    /// Path within source tree (relative), e.g. "agents/coder.md" or "skills/planning".
    pub source_path: PathBuf,
}

/// Discover items by conventional mars package layout.
pub fn discover_source(
    tree_path: &Path,
    fallback_name: Option<&str>,
) -> Result<Vec<DiscoveredItem>, MarsError> {
    let mut items = Vec::new();

    scan_agent_dir(
        tree_path,
        Path::new("agents"),
        &mut items,
        &mut HashSet::new(),
    )?;
    scan_skill_dir(
        tree_path,
        Path::new("skills"),
        &mut items,
        &mut HashSet::new(),
    )?;

    if items.is_empty() && tree_path.join("SKILL.md").is_file() {
        let name = fallback_name
            .map(String::from)
            .unwrap_or_else(|| package_basename(tree_path));
        items.push(DiscoveredItem {
            id: ItemId {
                kind: ItemKind::Skill,
                name: ItemName::from(name),
            },
            source_path: PathBuf::from("."),
        });
    }

    sort_items(&mut items);
    Ok(items)
}

/// Discover items using the Vercel-compatible fallback walk.
pub fn discover_fallback(
    package_root: &Path,
    source_name: Option<&str>,
) -> Result<Vec<DiscoveredItem>, MarsError> {
    if package_root.join("SKILL.md").is_file() {
        return Ok(vec![DiscoveredItem {
            id: ItemId {
                kind: ItemKind::Skill,
                name: ItemName::from(package_basename(package_root)),
            },
            source_path: PathBuf::from("."),
        }]);
    }

    let source_name = source_name.unwrap_or("unknown-source");
    let explicit_items = discover_manifest_declared_items(package_root, source_name)?;
    if !explicit_items.is_empty() {
        return finalize_items(source_name, explicit_items);
    }

    let heuristic_items = discover_heuristic_layer_items(package_root)?;
    finalize_items(source_name, heuristic_items)
}

/// Shared dispatcher for rooted-source discovery.
pub fn discover_resolved_source(
    package_root: &Path,
    source_name: Option<&str>,
) -> Result<Vec<DiscoveredItem>, MarsError> {
    if package_root.join("mars.toml").is_file() {
        discover_source(package_root, source_name)
    } else {
        discover_fallback(package_root, source_name)
    }
}

fn scan_skill_dir(
    package_root: &Path,
    relative_root: &Path,
    items: &mut Vec<DiscoveredItem>,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), MarsError> {
    let dir = package_root.join(relative_root);
    if !dir.is_dir() {
        return Ok(());
    }

    for path in read_dir_paths_sorted(&dir)? {
        if !path.is_dir() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|name| name.to_str())
            && name.starts_with('.')
        {
            continue;
        }
        let rel = relative_to(package_root, &path)?;
        register_skill_dir(package_root, &rel, items, visited)?;
    }

    Ok(())
}

fn scan_agent_dir(
    package_root: &Path,
    relative_root: &Path,
    items: &mut Vec<DiscoveredItem>,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), MarsError> {
    let dir = package_root.join(relative_root);
    if !dir.is_dir() {
        return Ok(());
    }

    for path in read_dir_paths_sorted(&dir)? {
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let rel = relative_to(package_root, &path)?;
        register_agent_file(&rel, items, visited);
    }

    Ok(())
}

fn scan_manifest_declared_path(
    package_root: &Path,
    declared_path: &DeclaredPath,
    items: &mut Vec<DiscoveredItem>,
) -> Result<(), MarsError> {
    let mut visited = HashSet::new();
    let candidate = package_root.join(&declared_path.relative_path);
    match declared_path.kind {
        ItemKind::Skill => {
            if candidate.join("SKILL.md").is_file() {
                register_skill_dir(
                    package_root,
                    &declared_path.relative_path,
                    items,
                    &mut visited,
                )?;
            } else if matches_container_root(&declared_path.relative_path, SKILL_CONTAINER_ROOTS) {
                scan_skill_dir(
                    package_root,
                    &declared_path.relative_path,
                    items,
                    &mut visited,
                )?;
            }
        }
        ItemKind::Agent => {
            if candidate.is_file()
                && candidate.extension().and_then(|ext| ext.to_str()) == Some("md")
            {
                register_agent_file(&declared_path.relative_path, items, &mut visited);
            } else if matches_container_root(&declared_path.relative_path, AGENT_CONTAINER_ROOTS) {
                scan_agent_dir(
                    package_root,
                    &declared_path.relative_path,
                    items,
                    &mut visited,
                )?;
            }
        }
    }

    Ok(())
}

fn register_skill_dir(
    package_root: &Path,
    relative_path: &Path,
    items: &mut Vec<DiscoveredItem>,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), MarsError> {
    let normalized = normalize_relative_path(relative_path);
    if !visited.insert(normalized.clone()) {
        return Ok(());
    }
    if !package_root.join(&normalized).join("SKILL.md").is_file() {
        return Ok(());
    }
    let name = normalized
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    items.push(DiscoveredItem {
        id: ItemId {
            kind: ItemKind::Skill,
            name: ItemName::from(name.to_string()),
        },
        source_path: normalized,
    });
    Ok(())
}

fn register_agent_file(
    relative_path: &Path,
    items: &mut Vec<DiscoveredItem>,
    visited: &mut HashSet<PathBuf>,
) {
    let normalized = normalize_relative_path(relative_path);
    if !visited.insert(normalized.clone()) {
        return;
    }
    let name = normalized
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    items.push(DiscoveredItem {
        id: ItemId {
            kind: ItemKind::Agent,
            name: ItemName::from(name.to_string()),
        },
        source_path: normalized,
    });
}

fn discover_manifest_declared_items(
    package_root: &Path,
    source_name: &str,
) -> Result<Vec<DiscoveredItem>, MarsError> {
    let mut items = Vec::new();
    for declared_path in collect_manifest_declared_paths(package_root, source_name)? {
        scan_manifest_declared_path(package_root, &declared_path, &mut items)?;
    }
    Ok(dedupe_items_by_path(items))
}

fn discover_heuristic_layer_items(package_root: &Path) -> Result<Vec<DiscoveredItem>, MarsError> {
    let candidates = collect_heuristic_candidates(package_root)?;
    let Some(min_layer) = candidates.iter().map(|candidate| candidate.layer).min() else {
        return Ok(Vec::new());
    };

    let items = candidates
        .into_iter()
        .filter(|candidate| candidate.layer == min_layer)
        .map(|candidate| candidate.item)
        .collect();
    let items = dedupe_items_by_path(items);
    Ok(dedupe_items_by_name_first_seen(items))
}

fn collect_heuristic_candidates(package_root: &Path) -> Result<Vec<LayeredCandidate>, MarsError> {
    let mut candidates = Vec::new();
    let mut queue = VecDeque::from([(package_root.to_path_buf(), 0usize)]);

    while let Some((base_dir, depth)) = queue.pop_front() {
        if depth > MAX_HEURISTIC_FS_DEPTH {
            continue;
        }

        let base_rel = if base_dir == package_root {
            PathBuf::new()
        } else {
            relative_to(package_root, &base_dir)?
        };
        collect_heuristic_candidates_at_base(package_root, &base_rel, &mut candidates)?;

        if depth == MAX_HEURISTIC_FS_DEPTH {
            continue;
        }

        for path in read_dir_paths_sorted(&base_dir)? {
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if RECURSIVE_SKIP_DIRS.contains(&name) {
                continue;
            }
            queue.push_back((path, depth + 1));
        }
    }

    Ok(candidates)
}

fn collect_heuristic_candidates_at_base(
    package_root: &Path,
    base_rel: &Path,
    candidates: &mut Vec<LayeredCandidate>,
) -> Result<(), MarsError> {
    collect_direct_skill_children(package_root, base_rel, candidates)?;
    for root in SKILL_CONTAINER_ROOTS {
        collect_skill_container_candidates(
            package_root,
            &join_relative(base_rel, Path::new(root)),
            candidates,
        )?;
    }
    for root in AGENT_CONTAINER_ROOTS {
        collect_agent_container_candidates(
            package_root,
            &join_relative(base_rel, Path::new(root)),
            candidates,
        )?;
    }
    Ok(())
}

fn collect_direct_skill_children(
    package_root: &Path,
    base_rel: &Path,
    candidates: &mut Vec<LayeredCandidate>,
) -> Result<(), MarsError> {
    let base_dir = package_root.join(base_rel);
    if !base_dir.is_dir() {
        return Ok(());
    }

    for path in read_dir_paths_sorted(&base_dir)? {
        if !path.is_dir() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|name| name.to_str())
            && name.starts_with('.')
        {
            continue;
        }
        let rel = relative_to(package_root, &path)?;
        if !path.join("SKILL.md").is_file() {
            continue;
        }
        candidates.push(LayeredCandidate::new(ItemKind::Skill, rel)?);
    }

    Ok(())
}

fn collect_skill_container_candidates(
    package_root: &Path,
    container_rel: &Path,
    candidates: &mut Vec<LayeredCandidate>,
) -> Result<(), MarsError> {
    let container_dir = package_root.join(container_rel);
    if !container_dir.is_dir() {
        return Ok(());
    }

    for path in read_dir_paths_sorted(&container_dir)? {
        if !path.is_dir() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|name| name.to_str())
            && name.starts_with('.')
        {
            continue;
        }
        if !path.join("SKILL.md").is_file() {
            continue;
        }
        let rel = relative_to(package_root, &path)?;
        candidates.push(LayeredCandidate::new(ItemKind::Skill, rel)?);
    }

    Ok(())
}

fn collect_agent_container_candidates(
    package_root: &Path,
    container_rel: &Path,
    candidates: &mut Vec<LayeredCandidate>,
) -> Result<(), MarsError> {
    let container_dir = package_root.join(container_rel);
    if !container_dir.is_dir() {
        return Ok(());
    }

    for path in read_dir_paths_sorted(&container_dir)? {
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let rel = relative_to(package_root, &path)?;
        candidates.push(LayeredCandidate::new(ItemKind::Agent, rel)?);
    }

    Ok(())
}

fn finalize_items(
    source_name: &str,
    mut items: Vec<DiscoveredItem>,
) -> Result<Vec<DiscoveredItem>, MarsError> {
    ensure_unique_names(source_name, &items)?;
    sort_items(&mut items);
    Ok(items)
}

fn dedupe_items_by_path(items: Vec<DiscoveredItem>) -> Vec<DiscoveredItem> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::with_capacity(items.len());
    for item in items {
        if seen.insert(item.source_path.clone()) {
            deduped.push(item);
        }
    }
    deduped
}

fn dedupe_items_by_name_first_seen(items: Vec<DiscoveredItem>) -> Vec<DiscoveredItem> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::with_capacity(items.len());
    for item in items {
        let key = (item.id.kind, item.id.name.to_string());
        if seen.insert(key) {
            deduped.push(item);
        }
    }
    deduped
}

fn collect_manifest_declared_paths(
    package_root: &Path,
    source_name: &str,
) -> Result<Vec<DeclaredPath>, MarsError> {
    let mut declared = Vec::new();
    for manifest in PLUGIN_MANIFESTS {
        let path = package_root.join(manifest);
        if !path.is_file() {
            continue;
        }
        let content = std::fs::read_to_string(&path)?;
        let json: Value = serde_json::from_str(&content).map_err(|e| MarsError::Source {
            source_name: source_name.to_string(),
            message: format!("failed to parse plugin manifest `{}`: {e}", path.display()),
        })?;
        declared.extend(parse_declared_paths(&json));
    }

    let mut resolved = Vec::new();
    let mut seen = HashSet::new();
    for raw in declared {
        if !raw.raw_path.starts_with("./") {
            continue;
        }
        let normalized = normalize_manifest_declared_path(&raw.raw_path).ok_or_else(|| {
            MarsError::ManifestDeclaredPathEscape {
                source_name: source_name.to_string(),
                manifest_path: raw.raw_path.display().to_string(),
                package_root: package_root.to_path_buf(),
            }
        })?;
        let candidate = package_root.join(&normalized);
        if !candidate.exists() {
            return Err(MarsError::ManifestDeclaredPathMissing {
                source_name: source_name.to_string(),
                manifest_path: raw.raw_path.display().to_string(),
                package_root: package_root.to_path_buf(),
            });
        }
        let canonical = dunce::canonicalize(&candidate).map_err(|_| {
            MarsError::ManifestDeclaredPathMissing {
                source_name: source_name.to_string(),
                manifest_path: raw.raw_path.display().to_string(),
                package_root: package_root.to_path_buf(),
            }
        })?;
        let canonical_root = dunce::canonicalize(package_root).map_err(|e| MarsError::Source {
            source_name: source_name.to_string(),
            message: format!(
                "failed to canonicalize package root `{}`: {e}",
                package_root.display()
            ),
        })?;
        if !canonical.starts_with(&canonical_root) {
            return Err(MarsError::ManifestDeclaredPathEscape {
                source_name: source_name.to_string(),
                manifest_path: raw.raw_path.display().to_string(),
                package_root: package_root.to_path_buf(),
            });
        }
        let rel = relative_to(package_root, &candidate)?;
        if seen.insert((raw.kind, rel.clone())) {
            resolved.push(DeclaredPath {
                kind: raw.kind,
                relative_path: rel,
            });
        }
    }
    Ok(resolved)
}

fn ensure_unique_names(source_name: &str, items: &[DiscoveredItem]) -> Result<(), MarsError> {
    let mut seen: HashMap<(ItemKind, String), PathBuf> = HashMap::new();
    for item in items {
        let key = (item.id.kind, item.id.name.to_string());
        if let Some(existing) = seen.insert(key.clone(), item.source_path.clone()) {
            return Err(MarsError::DiscoveryCollision {
                source_name: source_name.to_string(),
                kind: item.id.kind.to_string(),
                item_name: item.id.name.to_string(),
                path_a: existing,
                path_b: item.source_path.clone(),
            });
        }
    }
    Ok(())
}

fn relative_to(base: &Path, child: &Path) -> Result<PathBuf, MarsError> {
    child
        .strip_prefix(base)
        .map(|path| path.to_path_buf())
        .map_err(|_| MarsError::Source {
            source_name: "discover".to_string(),
            message: format!(
                "path `{}` is not under package root `{}`",
                child.display(),
                base.display()
            ),
        })
}

fn normalize_relative_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        normalized.push(component.as_os_str());
    }
    normalized
}

fn normalize_manifest_declared_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(seg) => normalized.push(seg),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if normalized.as_os_str().is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn package_basename(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("unknown-skill")
        .to_string()
}

fn read_dir_paths_sorted(dir: &Path) -> Result<Vec<PathBuf>, MarsError> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        paths.push(entry?.path());
    }
    paths.sort();
    Ok(paths)
}

fn join_relative(base: &Path, suffix: &Path) -> PathBuf {
    if base.as_os_str().is_empty() {
        suffix.to_path_buf()
    } else {
        base.join(suffix)
    }
}

fn matches_container_root(path: &Path, roots: &[&str]) -> bool {
    roots.iter().any(|root| path == Path::new(root))
}

fn parse_declared_paths(json: &Value) -> Vec<RawDeclaredPath> {
    let Some(map) = json.as_object() else {
        return Vec::new();
    };

    let mut declared = Vec::new();
    for key in MANIFEST_SKILL_KEYS {
        if let Some(value) = map.get(*key) {
            collect_declared_paths_from_value(ItemKind::Skill, value, &mut declared);
        }
    }
    for key in MANIFEST_AGENT_KEYS {
        if let Some(value) = map.get(*key) {
            collect_declared_paths_from_value(ItemKind::Agent, value, &mut declared);
        }
    }
    declared
}

fn collect_declared_paths_from_value(
    kind: ItemKind,
    value: &Value,
    declared: &mut Vec<RawDeclaredPath>,
) {
    match value {
        Value::String(path) => declared.push(RawDeclaredPath {
            kind,
            raw_path: PathBuf::from(path),
        }),
        Value::Array(values) => {
            for child in values {
                collect_declared_paths_from_value(kind, child, declared);
            }
        }
        Value::Object(map) => {
            if let Some(path) = map.get("path").and_then(|value| value.as_str()) {
                declared.push(RawDeclaredPath {
                    kind,
                    raw_path: PathBuf::from(path),
                });
            }
        }
        _ => {}
    }
}

fn split_segments(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(segment) => Some(segment.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

fn logical_layer(kind: ItemKind, relative_path: &Path) -> Result<usize, MarsError> {
    let segments = split_segments(relative_path);
    let default_layer = match kind {
        ItemKind::Skill => segments.len(),
        ItemKind::Agent => usize::MAX,
    };
    let container_roots = match kind {
        ItemKind::Skill => SKILL_CONTAINER_ROOTS,
        ItemKind::Agent => AGENT_CONTAINER_ROOTS,
    };

    let mut layer = default_layer;
    for root in container_roots {
        let root_segments: Vec<&str> = root.split('/').collect();
        if segments.len() < root_segments.len() + 1 {
            continue;
        }
        let start = segments.len() - 1 - root_segments.len();
        if segments[start..start + root_segments.len()]
            .iter()
            .map(String::as_str)
            .eq(root_segments.iter().copied())
        {
            layer = layer.min(start + 1);
        }
    }

    if layer == usize::MAX || layer == 0 || layer > MAX_FALLBACK_DEPTH {
        return Err(MarsError::Source {
            source_name: "discover".to_string(),
            message: format!(
                "invalid logical discovery layer for `{}`",
                relative_path.display()
            ),
        });
    }

    Ok(layer)
}

#[derive(Debug, Clone)]
struct RawDeclaredPath {
    kind: ItemKind,
    raw_path: PathBuf,
}

#[derive(Debug, Clone)]
struct DeclaredPath {
    kind: ItemKind,
    relative_path: PathBuf,
}

#[derive(Debug, Clone)]
struct LayeredCandidate {
    item: DiscoveredItem,
    layer: usize,
}

impl LayeredCandidate {
    fn new(kind: ItemKind, source_path: PathBuf) -> Result<Self, MarsError> {
        let item = match kind {
            ItemKind::Skill => DiscoveredItem {
                id: ItemId {
                    kind,
                    name: ItemName::from(
                        source_path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()
                            .to_string(),
                    ),
                },
                source_path: normalize_relative_path(&source_path),
            },
            ItemKind::Agent => DiscoveredItem {
                id: ItemId {
                    kind,
                    name: ItemName::from(
                        source_path
                            .file_stem()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()
                            .to_string(),
                    ),
                },
                source_path: normalize_relative_path(&source_path),
            },
        };

        Ok(Self {
            layer: logical_layer(kind, &item.source_path)?,
            item,
        })
    }
}

fn sort_items(items: &mut [DiscoveredItem]) {
    items.sort_by(|a, b| {
        a.id.cmp(&b.id)
            .then_with(|| a.source_path.cmp(&b.source_path))
    });
}

/// An installed item with parsed frontmatter metadata.
#[derive(Debug, Clone)]
pub struct InstalledItem {
    pub id: ItemId,
    /// Disk path (absolute) to the installed file/dir.
    pub path: PathBuf,
    /// Parsed frontmatter name (may differ from filename).
    pub frontmatter_name: Option<String>,
    /// Parsed frontmatter description.
    pub description: Option<String>,
    /// Skills referenced in frontmatter (agents only).
    pub skill_refs: Vec<String>,
}

/// Result of scanning an installed managed root.
#[derive(Debug, Clone)]
pub struct InstalledState {
    pub agents: Vec<InstalledItem>,
    pub skills: Vec<InstalledItem>,
}

/// Discover all installed agents and skills in a managed root.
pub fn discover_installed(root: &Path) -> Result<InstalledState, MarsError> {
    let mut agents = Vec::new();
    let mut skills = Vec::new();

    let mut scratch = Vec::new();
    let mut visited = HashSet::new();
    scan_agent_dir(root, Path::new("agents"), &mut scratch, &mut visited)?;
    for item in scratch.drain(..) {
        let path = root.join(&item.source_path);
        let (frontmatter_name, description, skill_refs) = parse_installed_frontmatter(&path);
        agents.push(InstalledItem {
            id: item.id,
            path,
            frontmatter_name,
            description,
            skill_refs,
        });
    }

    scan_skill_dir(root, Path::new("skills"), &mut scratch, &mut HashSet::new())?;
    for item in scratch.drain(..) {
        let path = root.join(&item.source_path);
        let skill_md = if item.source_path == Path::new(".") {
            root.join("SKILL.md")
        } else {
            path.join("SKILL.md")
        };
        let (frontmatter_name, description, _) = parse_installed_frontmatter(&skill_md);
        skills.push(InstalledItem {
            id: item.id,
            path,
            frontmatter_name,
            description,
            skill_refs: Vec::new(),
        });
    }

    sort_installed(&mut agents);
    sort_installed(&mut skills);
    Ok(InstalledState { agents, skills })
}

fn parse_installed_frontmatter(path: &Path) -> (Option<String>, Option<String>, Vec<String>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (None, None, Vec::new()),
    };
    match crate::frontmatter::parse(&content) {
        Ok(fm) => {
            let name = fm.name().map(str::to_owned);
            let description = fm
                .get("description")
                .and_then(|value| value.as_str())
                .map(str::to_owned);
            (name, description, fm.skills())
        }
        Err(_) => (None, None, Vec::new()),
    }
}

fn sort_installed(items: &mut [InstalledItem]) {
    items.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.path.cmp(&b.path)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn conventional_discovery_finds_agents_and_skills() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("agents")).unwrap();
        fs::create_dir_all(dir.path().join("skills/planning")).unwrap();
        fs::write(dir.path().join("agents/coder.md"), "# coder").unwrap();
        fs::write(dir.path().join("skills/planning/SKILL.md"), "# planning").unwrap();

        let items = discover_source(dir.path(), None).unwrap();
        assert_eq!(items.len(), 2);
        assert!(
            items
                .iter()
                .any(|item| item.source_path == Path::new("agents/coder.md"))
        );
        assert!(
            items
                .iter()
                .any(|item| item.source_path == Path::new("skills/planning"))
        );
    }

    #[test]
    fn dispatcher_prefers_conventional_when_manifest_exists() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("mars.toml"),
            "[package]\nname='demo'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("skills/planning")).unwrap();
        fs::write(dir.path().join("skills/planning/SKILL.md"), "# planning").unwrap();
        fs::create_dir_all(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/SKILL.md"), "# nested").unwrap();

        let items = discover_resolved_source(dir.path(), Some("demo")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_path, PathBuf::from("skills/planning"));
    }

    #[test]
    fn fallback_short_circuits_root_skill() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("SKILL.md"), "# root").unwrap();
        fs::create_dir_all(dir.path().join("skills/planning")).unwrap();
        fs::write(dir.path().join("skills/planning/SKILL.md"), "# planning").unwrap();

        let items = discover_fallback(dir.path(), Some("demo")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].id.name.as_str(),
            dir.path().file_name().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(items[0].source_path, PathBuf::from("."));
    }

    #[test]
    fn fallback_priority_scan_finds_skill_dirs_and_agents() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("skills/.experimental/find-skills")).unwrap();
        fs::create_dir_all(dir.path().join(".claude/agents")).unwrap();
        fs::write(
            dir.path().join("skills/.experimental/find-skills/SKILL.md"),
            "# skill",
        )
        .unwrap();
        fs::write(dir.path().join(".claude/agents/reviewer.md"), "# agent").unwrap();

        let items = discover_fallback(dir.path(), Some("demo")).unwrap();
        assert_eq!(items.len(), 2);
        assert!(
            items
                .iter()
                .any(|item| item.source_path == Path::new("skills/.experimental/find-skills"))
        );
        assert!(
            items
                .iter()
                .any(|item| item.source_path == Path::new(".claude/agents/reviewer.md"))
        );
    }

    #[test]
    fn conventional_root_skill_does_not_override_conventional_items() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("mars.toml"),
            "[package]\nname='demo'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::write(dir.path().join("SKILL.md"), "# root").unwrap();
        fs::create_dir_all(dir.path().join("skills/planning")).unwrap();
        fs::write(dir.path().join("skills/planning/SKILL.md"), "# planning").unwrap();

        let items = discover_resolved_source(dir.path(), Some("demo")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_path, PathBuf::from("skills/planning"));
    }

    #[test]
    fn fallback_manifest_paths_precede_heuristic_layers() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("top-level")).unwrap();
        fs::create_dir_all(dir.path().join("plugins/deep-skill")).unwrap();
        fs::write(dir.path().join("top-level/SKILL.md"), "# top").unwrap();
        fs::write(dir.path().join("plugins/deep-skill/SKILL.md"), "# deep").unwrap();
        fs::create_dir_all(dir.path().join(".claude-plugin")).unwrap();
        fs::write(
            dir.path().join(".claude-plugin/plugin.json"),
            r#"{"skills":[{"path":"./plugins/deep-skill"}]}"#,
        )
        .unwrap();

        let items = discover_fallback(dir.path(), Some("demo")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_path, PathBuf::from("plugins/deep-skill"));
    }

    #[test]
    fn fallback_dedupes_overlapping_manifest_and_container_paths() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("skills/planning")).unwrap();
        fs::write(dir.path().join("skills/planning/SKILL.md"), "# skill").unwrap();
        fs::create_dir_all(dir.path().join(".claude-plugin")).unwrap();
        fs::write(
            dir.path().join(".claude-plugin/plugin.json"),
            r#"{"skills":[{"path":"./skills/planning"}]}"#,
        )
        .unwrap();

        let items = discover_fallback(dir.path(), Some("demo")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_path, PathBuf::from("skills/planning"));
    }

    #[test]
    fn fallback_manifest_declares_agent_paths_without_heuristic_json_walk() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("agents")).unwrap();
        fs::write(dir.path().join("agents/reviewer.md"), "# reviewer").unwrap();
        fs::create_dir_all(dir.path().join(".claude-plugin")).unwrap();
        fs::write(
            dir.path().join(".claude-plugin/plugin.json"),
            r#"{"agents":[{"path":"./agents/reviewer.md"}],"metadata":{"agents":[{"path":"./ignore.md"}]}}"#,
        )
        .unwrap();

        let items = discover_fallback(dir.path(), Some("demo")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_path, PathBuf::from("agents/reviewer.md"));
    }

    #[test]
    fn fallback_prefers_first_match_after_visit_dedupe() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("skills/plan")).unwrap();
        fs::create_dir_all(dir.path().join("plan")).unwrap();
        fs::write(dir.path().join("skills/plan/SKILL.md"), "# skill a").unwrap();
        fs::write(dir.path().join("plan/SKILL.md"), "# skill b").unwrap();

        let items = discover_fallback(dir.path(), Some("demo")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_path, PathBuf::from("plan"));
    }

    #[test]
    fn fallback_prefers_first_mirrored_skill_match() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("skills/caveman")).unwrap();
        fs::create_dir_all(dir.path().join("caveman")).unwrap();
        fs::write(dir.path().join("skills/caveman/SKILL.md"), "# same").unwrap();
        fs::write(dir.path().join("caveman/SKILL.md"), "# same").unwrap();

        let items = discover_fallback(dir.path(), Some("demo")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_path, PathBuf::from("caveman"));
    }

    #[test]
    fn fallback_manifest_declared_escape_is_rejected() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".claude-plugin")).unwrap();
        fs::write(
            dir.path().join(".claude-plugin/plugin.json"),
            r#"{"skills":[{"path":"./../escape"}]}"#,
        )
        .unwrap();

        let err = discover_fallback(dir.path(), Some("demo")).unwrap_err();
        assert!(matches!(err, MarsError::ManifestDeclaredPathEscape { .. }));
    }

    #[test]
    fn fallback_selects_first_non_empty_logical_layer() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("top")).unwrap();
        fs::create_dir_all(dir.path().join("nested/deeper/skill")).unwrap();
        fs::write(dir.path().join("top/SKILL.md"), "# top").unwrap();
        fs::write(dir.path().join("nested/deeper/skill/SKILL.md"), "# skill").unwrap();

        let items = discover_fallback(dir.path(), Some("demo")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_path, PathBuf::from("top"));
    }

    #[test]
    fn fallback_groups_layout_variants_into_same_logical_layer() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("caveman")).unwrap();
        fs::create_dir_all(dir.path().join("skills/caveman")).unwrap();
        fs::write(dir.path().join("caveman/SKILL.md"), "# direct").unwrap();
        fs::write(dir.path().join("skills/caveman/SKILL.md"), "# container").unwrap();

        let items = discover_fallback(dir.path(), Some("demo")).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_path, PathBuf::from("caveman"));
    }

    #[test]
    fn discover_installed_reads_frontmatter() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("agents")).unwrap();
        fs::create_dir_all(dir.path().join("skills/planning")).unwrap();
        fs::write(
            dir.path().join("agents/coder.md"),
            "---\nname: coder\ndescription: test\nskills: [planning]\n---\n# Coder\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("skills/planning/SKILL.md"),
            "---\nname: planning\ndescription: test\n---\n# Planning\n",
        )
        .unwrap();

        let state = discover_installed(dir.path()).unwrap();
        assert_eq!(state.agents.len(), 1);
        assert_eq!(state.skills.len(), 1);
        assert_eq!(state.agents[0].skill_refs, vec!["planning"]);
        assert_eq!(
            state.skills[0].frontmatter_name.as_deref(),
            Some("planning")
        );
    }
}
