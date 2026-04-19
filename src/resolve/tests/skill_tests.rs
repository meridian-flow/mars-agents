use super::*;

#[test]
fn skill_not_found_has_requester_and_search_context() {
    let dir = TempDir::new().unwrap();
    let tree_a = dir.path().join("a");
    std::fs::create_dir_all(&tree_a).unwrap();
    write_minimal_package_marker(&tree_a);
    write_agent(&tree_a, "coder", &["missing-skill"]);

    let mut provider = MockProvider::new();
    provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
    provider.add_source("a", tree_a, None);

    let config = make_config(vec![(
        "a",
        git_spec("https://example.com/a.git", Some("v1.0.0")),
    )]);

    let err = resolve(&config, &provider, None, &default_options()).unwrap_err();
    match err {
        MarsError::Resolution(ResolutionError::SkillNotFound {
            skill,
            required_by,
            searched,
        }) => {
            assert_eq!(skill, "missing-skill");
            assert_eq!(required_by, "a/coder");
            assert_eq!(searched, vec!["a".to_string()]);
        }
        other => panic!("expected SkillNotFound, got {other:?}"),
    }
}

#[test]
fn excluded_skill_not_reintroduced_from_frontmatter_reference() {
    let dir = TempDir::new().unwrap();
    let tree = dir.path().join("a");
    std::fs::create_dir_all(&tree).unwrap();
    write_minimal_package_marker(&tree);
    write_agent(&tree, "coder", &["forbidden"]);
    write_skill_with_deps(&tree, "forbidden", &["missing-skill"]);

    let mut provider = MockProvider::new();
    provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
    provider.add_source("a", tree, None);

    let mut dependencies = IndexMap::new();
    dependencies.insert(
        SourceName::from("a"),
        EffectiveDependency {
            name: "a".into(),
            id: SourceId::git(SourceUrl::from("https://example.com/a.git")),
            spec: git_spec("https://example.com/a.git", Some("v1.0.0")),
            subpath: None,
            filter: FilterMode::Exclude(vec!["forbidden".into()]),
            rename: RenameMap::new(),
            is_overridden: false,
            original_git: None,
        },
    );
    let config = EffectiveConfig {
        dependencies,
        settings: Settings::default(),
    };

    let graph = resolve(&config, &provider, None, &default_options()).unwrap();
    assert!(graph.nodes.contains_key("a"));
}

#[test]
fn skill_resolution_prefers_dependency_closure_over_insertion_order() {
    let dir = TempDir::new().unwrap();
    let requester_tree = dir.path().join("requester");
    let sibling_tree = dir.path().join("sibling");
    let dep_tree = dir.path().join("my-dep");
    std::fs::create_dir_all(&requester_tree).unwrap();
    std::fs::create_dir_all(&sibling_tree).unwrap();
    std::fs::create_dir_all(&dep_tree).unwrap();

    write_minimal_package_marker(&requester_tree);
    write_minimal_package_marker(&sibling_tree);
    write_minimal_package_marker(&dep_tree);
    write_agent(&requester_tree, "coder", &["planning"]);
    write_skill(&sibling_tree, "planning");
    write_skill(&dep_tree, "planning");

    let requester_manifest = make_manifest(
        "requester",
        "1.0.0",
        vec![("my-dep", "https://example.com/my-dep.git", ">=1.0.0")],
    );

    let mut provider = MockProvider::new();
    provider.add_versions("https://example.com/requester.git", vec![(1, 0, 0)]);
    provider.add_versions("https://example.com/sibling.git", vec![(1, 0, 0)]);
    provider.add_versions("https://example.com/my-dep.git", vec![(1, 0, 0)]);
    provider.add_source("requester", requester_tree, Some(requester_manifest));
    provider.add_source("sibling", sibling_tree, None);
    provider.add_source("my-dep", dep_tree, None);

    let config = make_config(vec![
        ("sibling", git_spec("https://example.com/sibling.git", Some("v1.0.0"))),
        (
            "requester",
            git_spec("https://example.com/requester.git", Some("v1.0.0")),
        ),
    ]);

    let graph = resolve(&config, &provider, None, &default_options()).unwrap();

    let planning_include = FilterMode::Include {
        agents: Vec::new(),
        skills: vec![ItemName::from("planning")],
    };
    assert!(
        graph
            .filters
            .get("my-dep")
            .is_some_and(|filters| filters.contains(&planning_include)),
        "expected planning include filter on my-dep; got {:?}",
        graph.filters.get("my-dep")
    );
    assert!(
        !graph
            .filters
            .get("sibling")
            .is_some_and(|filters| filters.contains(&planning_include)),
        "did not expect planning include filter on sibling; got {:?}",
        graph.filters.get("sibling")
    );
}
