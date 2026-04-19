use super::*;


// ========== parse_version_constraint tests ==========

#[test]
fn parse_none_is_latest() {
    assert!(matches!(
        parse_version_constraint(None),
        VersionConstraint::Latest
    ));
}

#[test]
fn parse_empty_is_latest() {
    assert!(matches!(
        parse_version_constraint(Some("")),
        VersionConstraint::Latest
    ));
}

#[test]
fn parse_latest_string() {
    assert!(matches!(
        parse_version_constraint(Some("latest")),
        VersionConstraint::Latest
    ));
    assert!(matches!(
        parse_version_constraint(Some("LATEST")),
        VersionConstraint::Latest
    ));
}

#[test]
fn parse_exact_version() {
    match parse_version_constraint(Some("v1.2.3")) {
        VersionConstraint::Semver(req) => {
            assert!(req.matches(&Version::new(1, 2, 3)));
            assert!(!req.matches(&Version::new(1, 2, 4)));
        }
        other => panic!("expected Semver, got {other:?}"),
    }
}

#[test]
fn parse_major_version() {
    match parse_version_constraint(Some("v2")) {
        VersionConstraint::Semver(req) => {
            assert!(req.matches(&Version::new(2, 0, 0)));
            assert!(req.matches(&Version::new(2, 5, 3)));
            assert!(!req.matches(&Version::new(1, 9, 9)));
            assert!(!req.matches(&Version::new(3, 0, 0)));
        }
        other => panic!("expected Semver, got {other:?}"),
    }
}

#[test]
fn parse_major_minor_version() {
    match parse_version_constraint(Some("v2.1")) {
        VersionConstraint::Semver(req) => {
            assert!(req.matches(&Version::new(2, 1, 0)));
            assert!(req.matches(&Version::new(2, 1, 5)));
            assert!(!req.matches(&Version::new(2, 0, 9)));
            assert!(!req.matches(&Version::new(2, 2, 0)));
        }
        other => panic!("expected Semver, got {other:?}"),
    }
}

#[test]
fn parse_semver_req_gte() {
    match parse_version_constraint(Some(">=0.5.0")) {
        VersionConstraint::Semver(req) => {
            assert!(req.matches(&Version::new(0, 5, 0)));
            assert!(req.matches(&Version::new(1, 0, 0)));
            assert!(!req.matches(&Version::new(0, 4, 9)));
        }
        other => panic!("expected Semver, got {other:?}"),
    }
}

#[test]
fn parse_semver_req_caret() {
    match parse_version_constraint(Some("^2.0")) {
        VersionConstraint::Semver(req) => {
            assert!(req.matches(&Version::new(2, 0, 0)));
            assert!(req.matches(&Version::new(2, 9, 0)));
            assert!(!req.matches(&Version::new(3, 0, 0)));
        }
        other => panic!("expected Semver, got {other:?}"),
    }
}

#[test]
fn parse_semver_req_tilde() {
    match parse_version_constraint(Some("~1.2")) {
        VersionConstraint::Semver(req) => {
            assert!(req.matches(&Version::new(1, 2, 0)));
            assert!(req.matches(&Version::new(1, 2, 9)));
            assert!(!req.matches(&Version::new(1, 3, 0)));
        }
        other => panic!("expected Semver, got {other:?}"),
    }
}

#[test]
fn parse_branch_ref() {
    match parse_version_constraint(Some("main")) {
        VersionConstraint::RefPin(ref_name) => {
            assert_eq!(ref_name, "main");
        }
        other => panic!("expected RefPin, got {other:?}"),
    }
}

#[test]
fn parse_commit_ref() {
    match parse_version_constraint(Some("abc123def456")) {
        VersionConstraint::RefPin(ref_name) => {
            assert_eq!(ref_name, "abc123def456");
        }
        other => panic!("expected RefPin, got {other:?}"),
    }
}

#[test]
fn locked_version_preferred_when_satisfies_constraint() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("a");
    std::fs::create_dir_all(&tree).unwrap();

    let mut provider = MockProvider::new();
    provider.add_versions(
        "https://example.com/a.git",
        vec![(1, 0, 0), (1, 1, 0), (1, 2, 0)],
    );
    provider.add_source("a", tree, None);

    let config = make_config(vec![(
        "a",
        git_spec("https://example.com/a.git", Some("^1.0")),
    )]);

    // Lock file says v1.1.0
    let mut lock = LockFile::empty();
    lock.dependencies.insert(
        "a".into(),
        crate::lock::LockedSource {
            url: Some("https://example.com/a.git".into()),
            path: None,
            subpath: None,
            version: Some("v1.1.0".into()),
            commit: Some("abc".into()),
            tree_hash: None,
        },
    );

    let graph = resolve(&config, &provider, Some(&lock), &default_options()).unwrap();
    let node = &graph.nodes["a"];
    // Should prefer locked version 1.1.0 over MVS minimum 1.0.0
    assert_eq!(node.resolved_ref.version, Some(Version::new(1, 1, 0)));
}

#[test]
fn locked_version_ignored_when_constraint_changed() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("a");
    std::fs::create_dir_all(&tree).unwrap();

    let mut provider = MockProvider::new();
    provider.add_versions(
        "https://example.com/a.git",
        vec![(1, 0, 0), (2, 0, 0), (2, 1, 0)],
    );
    provider.add_source("a", tree, None);

    // Config now requires ^2.0
    let config = make_config(vec![(
        "a",
        git_spec("https://example.com/a.git", Some("^2.0")),
    )]);

    // Lock file says v1.0.0 — no longer satisfies ^2.0
    let mut lock = LockFile::empty();
    lock.dependencies.insert(
        "a".into(),
        crate::lock::LockedSource {
            url: Some("https://example.com/a.git".into()),
            path: None,
            subpath: None,
            version: Some("v1.0.0".into()),
            commit: Some("abc".into()),
            tree_hash: None,
        },
    );

    let graph = resolve(&config, &provider, Some(&lock), &default_options()).unwrap();
    let node = &graph.nodes["a"];
    // Locked version doesn't satisfy ^2.0, so MVS picks 2.0.0
    assert_eq!(node.resolved_ref.version, Some(Version::new(2, 0, 0)));
}

#[test]
fn locked_commit_is_used_when_reachable() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("a");
    std::fs::create_dir_all(&tree).unwrap();

    let mut provider = MockProvider::new();
    provider.add_versions("https://example.com/a.git", vec![(1, 0, 0), (1, 1, 0)]);
    provider.add_source("a", tree, None);

    let config = make_config(vec![(
        "a",
        git_spec("https://example.com/a.git", Some("^1.0")),
    )]);

    let locked_commit = "locked-sha-123";
    let mut lock = LockFile::empty();
    lock.dependencies.insert(
        "a".into(),
        crate::lock::LockedSource {
            url: Some("https://example.com/a.git".into()),
            path: None,
            subpath: None,
            version: Some("v1.1.0".into()),
            commit: Some(locked_commit.into()),
            tree_hash: None,
        },
    );

    let graph = resolve(&config, &provider, Some(&lock), &default_options()).unwrap();
    assert_eq!(
        graph.nodes["a"].resolved_ref.commit.as_deref(),
        Some(locked_commit)
    );
    assert_eq!(
        provider.seen_preferred_commits(),
        vec![Some(locked_commit.to_string())]
    );
}

#[test]
fn maximize_mode_ignores_locked_commit() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("a");
    std::fs::create_dir_all(&tree).unwrap();

    let mut provider = MockProvider::new();
    provider.add_versions(
        "https://example.com/a.git",
        vec![(1, 0, 0), (1, 1, 0), (1, 2, 0)],
    );
    provider.add_source("a", tree, None);

    let config = make_config(vec![(
        "a",
        git_spec("https://example.com/a.git", Some("^1.0")),
    )]);

    let unreachable_commit = "missing-locked-sha";
    provider.mark_unreachable_preferred_commit(unreachable_commit);

    let mut lock = LockFile::empty();
    lock.dependencies.insert(
        "a".into(),
        crate::lock::LockedSource {
            url: Some("https://example.com/a.git".into()),
            path: None,
            subpath: None,
            version: Some("v1.0.0".into()),
            commit: Some(unreachable_commit.into()),
            tree_hash: None,
        },
    );

    let options = ResolveOptions {
        maximize: true,
        upgrade_targets: HashSet::new(),
        bump_direct_constraints: false,
        frozen: false,
    };
    let graph = resolve(&config, &provider, Some(&lock), &options).unwrap();
    assert_eq!(
        graph.nodes["a"].resolved_ref.version,
        Some(Version::new(1, 2, 0))
    );
    assert_eq!(provider.seen_preferred_commits(), vec![None]);
}

#[test]
fn latest_resolves_to_newest() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("a");
    std::fs::create_dir_all(&tree).unwrap();

    let mut provider = MockProvider::new();
    provider.add_versions(
        "https://example.com/a.git",
        vec![(1, 0, 0), (2, 0, 0), (3, 0, 0)],
    );
    provider.add_source("a", tree, None);

    let config = make_config(vec![(
        "a",
        git_spec("https://example.com/a.git", Some("latest")),
    )]);

    let graph = resolve(&config, &provider, None, &default_options()).unwrap();
    let node = &graph.nodes["a"];
    // "latest" has no constraint, MVS picks minimum → 1.0.0
    // Actually, "latest" means any version. With MVS, minimum is 1.0.0.
    // But "latest" semantically means newest. Let me check the spec...
    // The spec says "@latest as any version (newest wins)"
    // So latest should pick the newest. Let me handle this in select_version.
    assert_eq!(node.resolved_ref.version, Some(Version::new(3, 0, 0)));
    assert_eq!(node.latest_version, Some(Version::new(3, 0, 0)));
}

#[test]
fn v2_resolves_to_major_range() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("a");
    std::fs::create_dir_all(&tree).unwrap();

    let mut provider = MockProvider::new();
    provider.add_versions(
        "https://example.com/a.git",
        vec![(1, 9, 0), (2, 0, 0), (2, 1, 0), (2, 5, 0), (3, 0, 0)],
    );
    provider.add_source("a", tree, None);

    let config = make_config(vec![(
        "a",
        git_spec("https://example.com/a.git", Some("v2")),
    )]);

    let graph = resolve(&config, &provider, None, &default_options()).unwrap();
    let node = &graph.nodes["a"];
    // v2 → >=2.0.0, <3.0.0, MVS picks minimum → 2.0.0
    assert_eq!(node.resolved_ref.version, Some(Version::new(2, 0, 0)));
}

#[test]
fn branch_ref_resolves_without_semver() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("a");
    std::fs::create_dir_all(&tree).unwrap();

    let mut provider = MockProvider::new();
    provider.add_source("a", tree, None);

    let config = make_config(vec![(
        "a",
        git_spec("https://example.com/a.git", Some("main")),
    )]);

    let graph = resolve(&config, &provider, None, &default_options()).unwrap();
    let node = &graph.nodes["a"];
    assert!(node.resolved_ref.version.is_none());
    assert!(node.latest_version.is_none());
    assert_eq!(node.resolved_ref.commit, Some("ref:main".into()));
}

#[test]
fn maximize_mode_picks_newest() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("a");
    std::fs::create_dir_all(&tree).unwrap();

    let mut provider = MockProvider::new();
    provider.add_versions(
        "https://example.com/a.git",
        vec![(1, 0, 0), (1, 5, 0), (1, 9, 0)],
    );
    provider.add_source("a", tree, None);

    let config = make_config(vec![(
        "a",
        git_spec("https://example.com/a.git", Some("^1.0")),
    )]);

    let options = ResolveOptions {
        maximize: true,
        upgrade_targets: HashSet::new(),
        bump_direct_constraints: false,
        frozen: false,
    };

    let graph = resolve(&config, &provider, None, &options).unwrap();
    let node = &graph.nodes["a"];
    assert_eq!(node.resolved_ref.version, Some(Version::new(1, 9, 0)));
}

#[test]
fn maximize_with_specific_targets() {
    let dir = TempDir::new().unwrap();
    let tree_a = dir.path().join("a");
    let tree_b = dir.path().join("b");
    std::fs::create_dir_all(&tree_a).unwrap();
    std::fs::create_dir_all(&tree_b).unwrap();

    let mut provider = MockProvider::new();
    provider.add_versions("https://example.com/a.git", vec![(1, 0, 0), (1, 5, 0)]);
    provider.add_versions("https://example.com/b.git", vec![(2, 0, 0), (2, 5, 0)]);
    provider.add_source("a", tree_a, None);
    provider.add_source("b", tree_b, None);

    let config = make_config(vec![
        ("a", git_spec("https://example.com/a.git", Some("^1.0"))),
        ("b", git_spec("https://example.com/b.git", Some("^2.0"))),
    ]);

    // Only upgrade "a", not "b"
    let options = ResolveOptions {
        maximize: true,
        upgrade_targets: HashSet::from(["a".into()]),
        bump_direct_constraints: false,
        frozen: false,
    };

    let graph = resolve(&config, &provider, None, &options).unwrap();
    // "a" should be maximized → 1.5.0
    assert_eq!(
        graph.nodes["a"].resolved_ref.version,
        Some(Version::new(1, 5, 0))
    );
    // "b" should use MVS → 2.0.0
    assert_eq!(
        graph.nodes["b"].resolved_ref.version,
        Some(Version::new(2, 0, 0))
    );
}

#[test]
fn bump_direct_constraints_ignores_direct_pin_for_target() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("a");
    std::fs::create_dir_all(&tree).unwrap();

    let mut provider = MockProvider::new();
    provider.add_versions("https://example.com/a.git", vec![(1, 0, 0), (2, 0, 0)]);
    provider.add_source("a", tree, None);

    let config = make_config(vec![(
        "a",
        git_spec("https://example.com/a.git", Some("v1.0.0")),
    )]);

    let options = ResolveOptions {
        maximize: true,
        upgrade_targets: HashSet::from([SourceName::from("a")]),
        bump_direct_constraints: true,
        frozen: false,
    };

    let graph = resolve(&config, &provider, None, &options).unwrap();
    assert_eq!(
        graph.nodes["a"].resolved_ref.version,
        Some(Version::new(2, 0, 0))
    );
}

#[test]
fn no_available_versions_falls_back_to_head() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("a");
    std::fs::create_dir_all(&tree).unwrap();

    let mut provider = MockProvider::new();
    // No versions registered → empty list
    provider.add_source("a", tree, None);

    let config = make_config(vec![("a", git_spec("https://example.com/a.git", None))]);

    let graph = resolve(&config, &provider, None, &default_options()).unwrap();
    let node = &graph.nodes["a"];
    assert!(node.resolved_ref.version.is_none());
    assert_eq!(node.resolved_ref.commit, Some("ref:HEAD".into()));
}

#[test]
fn untagged_source_uses_locked_commit_when_available() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("a");
    std::fs::create_dir_all(&tree).unwrap();

    let mut provider = MockProvider::new();
    provider.add_source("a", tree, None);

    let config = make_config(vec![("a", git_spec("https://example.com/a.git", None))]);

    let locked_commit = "locked-untagged-sha";
    let mut lock = LockFile::empty();
    lock.dependencies.insert(
        "a".into(),
        crate::lock::LockedSource {
            url: Some("https://example.com/a.git".into()),
            path: None,
            subpath: None,
            version: None,
            commit: Some(locked_commit.into()),
            tree_hash: None,
        },
    );

    let graph = resolve(&config, &provider, Some(&lock), &default_options()).unwrap();
    assert_eq!(
        graph.nodes["a"].resolved_ref.commit.as_deref(),
        Some(locked_commit)
    );
    assert_eq!(
        provider.seen_preferred_commits(),
        vec![Some(locked_commit.to_string())]
    );
}

#[test]
fn untagged_source_falls_back_to_head_when_locked_commit_unreachable() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("a");
    std::fs::create_dir_all(&tree).unwrap();

    let mut provider = MockProvider::new();
    provider.add_source("a", tree, None);

    let config = make_config(vec![("a", git_spec("https://example.com/a.git", None))]);

    let unreachable_commit = "missing-locked-sha";
    provider.mark_unreachable_preferred_commit(unreachable_commit);

    let mut lock = LockFile::empty();
    lock.dependencies.insert(
        "a".into(),
        crate::lock::LockedSource {
            url: Some("https://example.com/a.git".into()),
            path: None,
            subpath: None,
            version: None,
            commit: Some(unreachable_commit.into()),
            tree_hash: None,
        },
    );

    let graph = resolve(&config, &provider, Some(&lock), &default_options()).unwrap();
    assert_eq!(
        graph.nodes["a"].resolved_ref.commit.as_deref(),
        Some("ref:HEAD")
    );
    assert_eq!(
        provider.seen_preferred_commits(),
        vec![Some(unreachable_commit.to_string()), None]
    );
}

#[test]
fn frozen_mode_errors_for_untagged_locked_commit_unreachable() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("a");
    std::fs::create_dir_all(&tree).unwrap();

    let mut provider = MockProvider::new();
    provider.add_source("a", tree, None);

    let config = make_config(vec![("a", git_spec("https://example.com/a.git", None))]);

    let unreachable_commit = "missing-locked-sha";
    provider.mark_unreachable_preferred_commit(unreachable_commit);

    let mut lock = LockFile::empty();
    lock.dependencies.insert(
        "a".into(),
        crate::lock::LockedSource {
            url: Some("https://example.com/a.git".into()),
            path: None,
            subpath: None,
            version: None,
            commit: Some(unreachable_commit.into()),
            tree_hash: None,
        },
    );

    let options = ResolveOptions {
        frozen: true,
        ..default_options()
    };
    let result = resolve(&config, &provider, Some(&lock), &options);
    assert!(matches!(
        result,
        Err(MarsError::LockedCommitUnreachable { .. })
    ));
    assert_eq!(
        provider.seen_preferred_commits(),
        vec![Some(unreachable_commit.to_string())]
    );
}
