use super::*;
use crate::config::GitSpec;
use crate::lock::ItemKind;
use semver::Version;
use std::path::PathBuf;

fn semver_constraint(req: &str) -> VersionConstraint {
    VersionConstraint::Semver(req.parse().expect("valid semver requirement"))
}

fn resolved_ref(
    package: &str,
    version: Option<&str>,
    version_tag: Option<&str>,
    commit: Option<&str>,
    tree_path: &str,
) -> ResolvedRef {
    ResolvedRef {
        source_name: SourceName::from(package),
        version: version.map(|v| Version::parse(v).expect("valid version")),
        version_tag: version_tag.map(str::to_string),
        commit: commit.map(|c| c.into()),
        tree_path: PathBuf::from(tree_path),
    }
}

#[test]
fn visited_set_not_seen() {
    let visited = VisitedSet::new();
    let package = SourceName::from("alpha");
    let item = ItemName::from("coder");

    let result = visited.check_version(&package, &item, &VersionConstraint::Latest);
    assert!(matches!(result, VersionCheckResult::NotSeen));
}

#[test]
fn visited_set_same_version() {
    let mut visited = VisitedSet::new();
    let package = SourceName::from("alpha");
    let item = ItemName::from("coder");
    let constraint = semver_constraint("^1.2");
    visited.insert(
        package.clone(),
        item.clone(),
        constraint.clone(),
        resolved_ref(
            "alpha",
            Some("1.2.3"),
            Some("v1.2.3"),
            Some("abc123"),
            "/tmp/alpha",
        ),
    );

    let result = visited.check_version(&package, &item, &constraint);
    assert!(matches!(result, VersionCheckResult::SameVersion));

    let entries: Vec<_> = visited.iter().collect();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, &(package, item));
}

#[test]
fn visited_set_different_version() {
    let mut visited = VisitedSet::new();
    let package = SourceName::from("alpha");
    let item = ItemName::from("coder");

    visited.insert(
        package.clone(),
        item.clone(),
        semver_constraint("^1.0"),
        resolved_ref(
            "alpha",
            Some("1.4.0"),
            Some("v1.4.0"),
            Some("abc123"),
            "/tmp/alpha",
        ),
    );

    let requested = semver_constraint("^2.0");
    let result = visited.check_version(&package, &item, &requested);
    match result {
        VersionCheckResult::DifferentVersion {
            existing,
            requested,
        } => {
            assert!(matches!(existing, VersionConstraint::Semver(_)));
            assert!(matches!(requested, VersionConstraint::Semver(_)));
            assert_eq!(
                existing.compatible_with(&requested),
                CompatibilityResult::Conflicting
            );
        }
        other => panic!("expected DifferentVersion, got {other:?}"),
    }
}

#[test]
fn visited_set_potentially_conflicting_version() {
    let mut visited = VisitedSet::new();
    let package = SourceName::from("alpha");
    let item = ItemName::from("coder");

    visited.insert(
        package.clone(),
        item.clone(),
        VersionConstraint::Latest,
        resolved_ref(
            "alpha",
            Some("2.0.0"),
            Some("v2.0.0"),
            Some("abc123"),
            "/tmp/alpha",
        ),
    );

    let requested = semver_constraint("^1.0");
    let result = visited.check_version(&package, &item, &requested);
    match result {
        VersionCheckResult::PotentiallyConflicting {
            existing,
            requested,
        } => {
            assert!(matches!(existing, VersionConstraint::Latest));
            assert!(matches!(requested, VersionConstraint::Semver(_)));
            assert_eq!(
                existing.compatible_with(&requested),
                CompatibilityResult::PotentiallyConflicting
            );
        }
        other => panic!("expected PotentiallyConflicting, got {other:?}"),
    }
}

fn pending_item(is_local: bool) -> PendingItem {
    PendingItem {
        package: SourceName::from("alpha"),
        item: ItemName::from("coder"),
        kind: ItemKind::Agent,
        constraint: semver_constraint("^1.0"),
        required_by: "mars.toml".to_string(),
        is_local,
        spec: SourceSpec::Git(GitSpec {
            url: SourceUrl::from("https://example.com/alpha.git"),
            version: Some("v1.0.0".to_string()),
        }),
    }
}

#[test]
fn apply_item_version_policy_skips_local_conflict() {
    let pending = pending_item(true);
    let mut diag = DiagnosticCollector::new();
    let action = apply_item_version_policy(
        &pending,
        VersionCheckResult::DifferentVersion {
            existing: semver_constraint("^1.0"),
            requested: semver_constraint("^2.0"),
        },
        &mut diag,
    )
    .expect("local conflicting versions should be skipped");

    assert!(matches!(action, VersionAction::Skip));
}

#[test]
fn apply_item_version_policy_warns_on_potential_drift() {
    let pending = pending_item(false);
    let mut diag = DiagnosticCollector::new();
    let action = apply_item_version_policy(
        &pending,
        VersionCheckResult::PotentiallyConflicting {
            existing: VersionConstraint::Latest,
            requested: semver_constraint("^1.0"),
        },
        &mut diag,
    )
    .expect("potential conflicts should warn and continue");

    assert!(matches!(action, VersionAction::Skip));
    let diagnostics = diag.drain();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code, "potential-version-drift");
    assert!(
        diagnostics[0]
            .message
            .contains("potential version drift: item 'coder' from 'alpha'"),
        "unexpected warning text: {}",
        diagnostics[0].message,
    );
}

#[test]
fn apply_item_version_policy_errors_on_non_local_conflict() {
    let pending = pending_item(false);
    let mut diag = DiagnosticCollector::new();
    let err = apply_item_version_policy(
        &pending,
        VersionCheckResult::DifferentVersion {
            existing: semver_constraint("^1.0"),
            requested: semver_constraint("^2.0"),
        },
        &mut diag,
    )
    .expect_err("non-local conflicting versions should error");

    match err {
        ResolutionError::ItemVersionConflict {
            item,
            package,
            existing,
            requested,
            chain,
        } => {
            assert_eq!(item, "coder");
            assert_eq!(package, "alpha");
            assert_eq!(existing, "^1.0");
            assert_eq!(requested, "^2.0");
            assert_eq!(chain, "mars.toml");
        }
        other => panic!("expected ItemVersionConflict, got {other:?}"),
    }
}

#[test]
fn package_versions_first_insert() {
    let mut versions = PackageVersions::new();
    let package = SourceName::from("alpha");
    let resolved = resolved_ref(
        "alpha",
        Some("1.0.0"),
        Some("v1.0.0"),
        Some("abc123"),
        "/tmp/alpha",
    );

    assert!(
        versions
            .check_or_insert(
                &package,
                &resolved,
                &VersionConstraint::Latest,
                "mars.toml",
                false,
            )
            .is_ok()
    );
}

#[test]
fn package_versions_same_version_reuse() {
    let mut versions = PackageVersions::new();
    let package = SourceName::from("alpha");
    let resolved = resolved_ref(
        "alpha",
        Some("1.0.0"),
        Some("v1.0.0"),
        Some("abc123"),
        "/tmp/alpha",
    );
    versions
        .check_or_insert(
            &package,
            &resolved,
            &VersionConstraint::Latest,
            "mars.toml",
            false,
        )
        .expect("initial insert should succeed");

    assert!(
        versions
            .check_or_insert(
                &package,
                &resolved,
                &VersionConstraint::Latest,
                "agent:coder",
                false,
            )
            .is_ok()
    );
}

#[test]
fn package_versions_conflict() {
    let mut versions = PackageVersions::new();
    let package = SourceName::from("alpha");
    let existing = resolved_ref(
        "alpha",
        Some("1.0.0"),
        Some("v1.0.0"),
        Some("abc123"),
        "/tmp/alpha-v1",
    );
    let requested = resolved_ref(
        "alpha",
        Some("2.0.0"),
        Some("v2.0.0"),
        Some("def456"),
        "/tmp/alpha-v2",
    );
    versions
        .check_or_insert(
            &package,
            &existing,
            &semver_constraint("^1.0"),
            "mars.toml",
            false,
        )
        .expect("initial insert should succeed");

    let err = versions
        .check_or_insert(
            &package,
            &requested,
            &semver_constraint("^1.0"),
            "agent:coder",
            false,
        )
        .expect_err("second insert with different resolved ref should fail");

    match err {
        ResolutionError::PackageVersionConflict {
            package,
            existing,
            requested,
            chain,
        } => {
            assert_eq!(package, "alpha");
            assert!(existing.contains("required by mars.toml"));
            assert!(requested.contains("required by agent:coder"));
            assert_eq!(chain, "agent:coder");
        }
        other => panic!("expected PackageVersionConflict, got {other:?}"),
    }
}

#[test]
fn package_versions_conflicting_constraints() {
    let mut versions = PackageVersions::new();
    let package = SourceName::from("alpha");
    let resolved = resolved_ref(
        "alpha",
        Some("1.0.0"),
        Some("v1.0.0"),
        Some("abc123"),
        "/tmp/alpha",
    );
    versions
        .check_or_insert(
            &package,
            &resolved,
            &semver_constraint("^1.0"),
            "mars.toml",
            false,
        )
        .expect("initial insert should succeed");

    let err = versions
        .check_or_insert(
            &package,
            &resolved,
            &semver_constraint("^2.0"),
            "agent:coder",
            false,
        )
        .expect_err("conflicting package constraints should fail");

    match err {
        ResolutionError::PackageVersionConflict {
            package,
            existing,
            requested,
            chain,
        } => {
            assert_eq!(package, "alpha");
            assert!(existing.contains("^1.0"));
            assert!(requested.contains("^2.0"));
            assert_eq!(chain, "agent:coder");
        }
        other => panic!("expected PackageVersionConflict, got {other:?}"),
    }
}

#[test]
fn package_versions_local_conflict_bypassed() {
    let mut versions = PackageVersions::new();
    let package = SourceName::from("alpha");
    let existing = resolved_ref(
        "alpha",
        Some("1.0.0"),
        Some("v1.0.0"),
        Some("abc123"),
        "/tmp/alpha-v1",
    );
    let requested = resolved_ref(
        "alpha",
        Some("2.0.0"),
        Some("v2.0.0"),
        Some("def456"),
        "/tmp/alpha-v2",
    );

    versions
        .check_or_insert(
            &package,
            &existing,
            &semver_constraint("^1.0"),
            "mars.toml",
            false,
        )
        .expect("initial insert should succeed");

    assert!(
        versions
            .check_or_insert(
                &package,
                &requested,
                &semver_constraint("^2.0"),
                "agent:coder",
                true,
            )
            .is_ok(),
        "local package conflicts must be bypassed",
    );
}

#[test]
fn pending_item_scaffolding_fields_roundtrip() {
    let pending = PendingItem {
        package: SourceName::from("alpha"),
        item: ItemName::from("coder"),
        kind: ItemKind::Agent,
        constraint: VersionConstraint::Latest,
        required_by: "mars.toml".to_string(),
        is_local: false,
        spec: SourceSpec::Git(GitSpec {
            url: SourceUrl::from("https://example.com/alpha.git"),
            version: Some("v1.2.3".to_string()),
        }),
    };

    assert_eq!(pending.package, "alpha");
    assert_eq!(pending.item, "coder");
    assert_eq!(pending.kind, ItemKind::Agent);
    assert!(matches!(pending.constraint, VersionConstraint::Latest));
    assert_eq!(pending.required_by, "mars.toml");
    assert!(!pending.is_local);
    assert!(matches!(pending.spec, SourceSpec::Git(_)));
}
