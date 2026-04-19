#![cfg(test)]

mod filter_tests;
mod integration_tests;
mod skill_tests;
mod tracker_tests;
mod version_tests;

use super::*;
use crate::config::{
    EffectiveConfig, EffectiveDependency, FilterConfig, FilterMode, GitSpec, Manifest,
    ManifestDep, PackageInfo, Settings, SourceSpec,
};
use crate::diagnostic::DiagnosticLevel;
use crate::types::{RenameMap, SourceId, SourceName, SourceSubpath, SourceUrl};
use indexmap::IndexMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tempfile::TempDir;

// ========== Mock SourceProvider ==========

/// Mock provider for testing the resolver without real git repos.
pub(crate) struct MockProvider {
    /// url → sorted available versions
    versions: HashMap<String, Vec<AvailableVersion>>,
    /// source tree paths keyed by source name (pre-created temp dirs)
    trees: HashMap<String, PathBuf>,
    /// Manifests to return for specific source trees
    manifests: HashMap<PathBuf, Option<Manifest>>,
    /// Preferred commits that should simulate an unreachable lock replay.
    unreachable_preferred_commits: HashSet<String>,
    /// Captures preferred-commit hints passed by the resolver.
    seen_preferred_commits: RefCell<Vec<Option<String>>>,
    /// Number of fetches keyed by source name.
    fetch_counts: RefCell<HashMap<String, usize>>,
}

impl MockProvider {
    fn new() -> Self {
        MockProvider {
            versions: HashMap::new(),
            trees: HashMap::new(),
            manifests: HashMap::new(),
            unreachable_preferred_commits: HashSet::new(),
            seen_preferred_commits: RefCell::new(Vec::new()),
            fetch_counts: RefCell::new(HashMap::new()),
        }
    }

    /// Register available versions for a URL.
    fn add_versions(&mut self, url: &str, versions: Vec<(u64, u64, u64)>) {
        let avs: Vec<AvailableVersion> = versions
            .into_iter()
            .map(|(major, minor, patch)| AvailableVersion {
                tag: format!("v{major}.{minor}.{patch}"),
                version: Version::new(major, minor, patch),
                commit_id: "0000000000000000000000000000000000000000".to_string(),
            })
            .collect();
        self.versions.insert(url.to_string(), avs);
    }

    /// Register a source tree for a source name, with optional manifest.
    fn add_source(&mut self, name: &str, tree_path: PathBuf, manifest: Option<Manifest>) {
        if let Some(ref m) = manifest {
            self.manifests.insert(tree_path.clone(), Some(m.clone()));
        } else {
            self.manifests.insert(tree_path.clone(), None);
        }
        self.trees.insert(name.to_string(), tree_path);
    }

    fn mark_unreachable_preferred_commit(&mut self, commit: &str) {
        self.unreachable_preferred_commits
            .insert(commit.to_string());
    }

    fn seen_preferred_commits(&self) -> Vec<Option<String>> {
        self.seen_preferred_commits.borrow().clone()
    }

    fn fetch_count(&self, source_name: &str) -> usize {
        self.fetch_counts
            .borrow()
            .get(source_name)
            .copied()
            .unwrap_or(0)
    }

    fn bump_fetch_count(&self, source_name: &str) {
        let mut counts = self.fetch_counts.borrow_mut();
        let entry = counts.entry(source_name.to_string()).or_insert(0);
        *entry += 1;
    }
}

impl VersionLister for MockProvider {
    fn list_versions(&self, url: &SourceUrl) -> Result<Vec<AvailableVersion>, MarsError> {
        Ok(self.versions.get(url.as_ref()).cloned().unwrap_or_default())
    }
}

impl SourceFetcher for MockProvider {
    fn fetch_git_version(
        &self,
        url: &SourceUrl,
        version: &AvailableVersion,
        source_name: &str,
        preferred_commit: Option<&str>,
        _diag: &mut DiagnosticCollector,
    ) -> Result<ResolvedRef, MarsError> {
        self.bump_fetch_count(source_name);
        self.seen_preferred_commits
            .borrow_mut()
            .push(preferred_commit.map(str::to_string));

        if let Some(commit) = preferred_commit
            && self.unreachable_preferred_commits.contains(commit)
        {
            return Err(MarsError::LockedCommitUnreachable {
                commit: commit.to_string(),
                url: url.to_string(),
            });
        }

        let tree_path = self.trees.get(source_name).cloned().unwrap_or_default();
        Ok(ResolvedRef {
            source_name: source_name.into(),
            version: Some(version.version.clone()),
            version_tag: Some(version.tag.clone()),
            commit: Some(
                preferred_commit
                    .map(|c| c.into())
                    .unwrap_or_else(|| "mock-commit".into()),
            ),
            tree_path,
        })
    }

    fn fetch_git_ref(
        &self,
        url: &SourceUrl,
        ref_name: &str,
        source_name: &str,
        preferred_commit: Option<&str>,
        _diag: &mut DiagnosticCollector,
    ) -> Result<ResolvedRef, MarsError> {
        self.bump_fetch_count(source_name);
        self.seen_preferred_commits
            .borrow_mut()
            .push(preferred_commit.map(str::to_string));

        if let Some(commit) = preferred_commit
            && self.unreachable_preferred_commits.contains(commit)
        {
            return Err(MarsError::LockedCommitUnreachable {
                commit: commit.to_string(),
                url: url.to_string(),
            });
        }

        let tree_path = self.trees.get(source_name).cloned().unwrap_or_default();
        Ok(ResolvedRef {
            source_name: source_name.into(),
            version: None,
            version_tag: None,
            commit: Some(
                preferred_commit
                    .map(|c| c.into())
                    .unwrap_or_else(|| format!("ref:{ref_name}").into()),
            ),
            tree_path,
        })
    }

    fn fetch_path(
        &self,
        path: &Path,
        source_name: &str,
        _diag: &mut DiagnosticCollector,
    ) -> Result<ResolvedRef, MarsError> {
        self.bump_fetch_count(source_name);
        Ok(ResolvedRef {
            source_name: source_name.into(),
            version: None,
            version_tag: None,
            commit: None,
            tree_path: path.to_path_buf(),
        })
    }
}

impl ManifestReader for MockProvider {
    fn read_manifest(
        &self,
        source_tree: &Path,
        _diag: &mut DiagnosticCollector,
    ) -> Result<Option<Manifest>, MarsError> {
        Ok(self.manifests.get(source_tree).cloned().unwrap_or(None))
    }
}

// ========== Helper functions ==========

fn make_config(sources: Vec<(&str, SourceSpec)>) -> EffectiveConfig {
    let mut map = IndexMap::new();
    for (name, spec) in sources {
        map.insert(
            name.into(),
            EffectiveDependency {
                name: name.into(),
                id: source_id_for_spec(&spec, None),
                spec,
                subpath: None,
                filter: FilterMode::All,
                rename: RenameMap::new(),
                is_overridden: false,
                original_git: None,
            },
        );
    }
    EffectiveConfig {
        dependencies: map,
        settings: Settings::default(),
    }
}

fn git_spec(url: &str, version: Option<&str>) -> SourceSpec {
    SourceSpec::Git(GitSpec {
        url: SourceUrl::from(url),
        version: version.map(|s| s.to_string()),
    })
}

fn make_manifest(name: &str, version: &str, deps: Vec<(&str, &str, &str)>) -> Manifest {
    let mut dependencies = IndexMap::new();
    for (dep_name, dep_url, dep_ver) in deps {
        dependencies.insert(
            dep_name.to_string(),
            ManifestDep {
                url: SourceUrl::from(dep_url),
                subpath: None,
                version: Some(dep_ver.to_string()),
                filter: crate::config::FilterConfig::default(),
            },
        );
    }
    Manifest {
        package: PackageInfo {
            name: name.to_string(),
            version: version.to_string(),
            description: None,
        },
        dependencies,
        models: indexmap::IndexMap::new(),
    }
}

fn make_manifest_with_filters(
    name: &str,
    version: &str,
    deps: Vec<(&str, &str, &str, FilterConfig)>,
) -> Manifest {
    let mut dependencies = IndexMap::new();
    for (dep_name, dep_url, dep_ver, dep_filter) in deps {
        dependencies.insert(
            dep_name.to_string(),
            ManifestDep {
                url: SourceUrl::from(dep_url),
                subpath: None,
                version: Some(dep_ver.to_string()),
                filter: dep_filter,
            },
        );
    }
    Manifest {
        package: PackageInfo {
            name: name.to_string(),
            version: version.to_string(),
            description: None,
        },
        dependencies,
        models: indexmap::IndexMap::new(),
    }
}

fn default_options() -> ResolveOptions {
    ResolveOptions::default()
}

fn resolve(
    config: &EffectiveConfig,
    provider: &dyn SourceProvider,
    locked: Option<&LockFile>,
    options: &ResolveOptions,
) -> Result<ResolvedGraph, MarsError> {
    resolve_with_diagnostics(config, provider, locked, options).0
}

fn resolve_with_diagnostics(
    config: &EffectiveConfig,
    provider: &dyn SourceProvider,
    locked: Option<&LockFile>,
    options: &ResolveOptions,
) -> (
    Result<ResolvedGraph, MarsError>,
    Vec<crate::diagnostic::Diagnostic>,
) {
    let mut diag = DiagnosticCollector::new();
    let result = super::resolve(config, provider, locked, options, &mut diag);
    (result, diag.drain())
}

fn write_minimal_package_marker(tree: &Path) {
    std::fs::write(
        tree.join("mars.toml"),
        "[package]\nname = \"pkg\"\nversion = \"1.0.0\"\n",
    )
    .expect("write mars.toml");
}

fn write_skill(tree: &Path, name: &str) {
    let dir = tree.join("skills").join(name);
    std::fs::create_dir_all(&dir).expect("create skill dir");
    std::fs::write(dir.join("SKILL.md"), "---\n---\n").expect("write SKILL.md");
}

fn write_skill_with_deps(tree: &Path, name: &str, skills: &[&str]) {
    let dir = tree.join("skills").join(name);
    std::fs::create_dir_all(&dir).expect("create skill dir");
    let frontmatter = if skills.is_empty() {
        "---\n---\n".to_string()
    } else {
        format!("---\nskills: [{}]\n---\n", skills.join(", "))
    };
    std::fs::write(dir.join("SKILL.md"), frontmatter).expect("write SKILL.md");
}

fn write_agent(tree: &Path, name: &str, skills: &[&str]) {
    let agents = tree.join("agents");
    std::fs::create_dir_all(&agents).expect("create agents dir");
    let frontmatter = if skills.is_empty() {
        "---\n---\n".to_string()
    } else {
        format!("---\nskills: [{}]\n---\n", skills.join(", "))
    };
    std::fs::write(agents.join(format!("{name}.md")), frontmatter).expect("write agent");
}

fn source_id_for_spec(spec: &SourceSpec, subpath: Option<SourceSubpath>) -> SourceId {
    match spec {
        SourceSpec::Git(g) => SourceId::git_with_subpath(g.url.clone(), subpath),
        SourceSpec::Path(path) => SourceId::Path {
            canonical: path.clone(),
            subpath,
        },
    }
}

fn dummy_ref(name: &str) -> ResolvedRef {
    ResolvedRef {
        source_name: name.into(),
        version: None,
        version_tag: None,
        commit: None,
        tree_path: PathBuf::new(),
    }
}

fn dummy_rooted_ref() -> RootedSourceRef {
    RootedSourceRef {
        checkout_root: PathBuf::new(),
        package_root: PathBuf::new(),
    }
}
