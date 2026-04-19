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
