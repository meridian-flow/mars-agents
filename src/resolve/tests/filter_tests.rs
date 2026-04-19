use super::*;


#[test]
fn transitive_dep_filter_is_collected() {
    let dir = TempDir::new().unwrap();
    let tree_a = dir.path().join("a");
    let tree_dep = dir.path().join("dep");
    std::fs::create_dir_all(&tree_a).unwrap();
    std::fs::create_dir_all(&tree_dep).unwrap();

    let manifest_a = make_manifest_with_filters(
        "a",
        "1.0.0",
        vec![(
            "dep",
            "https://example.com/dep.git",
            ">=1.0.0",
            FilterConfig {
                skills: Some(vec!["frontend-design".into()]),
                ..FilterConfig::default()
            },
        )],
    );

    let mut provider = MockProvider::new();
    provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
    provider.add_versions("https://example.com/dep.git", vec![(1, 0, 0)]);
    provider.add_source("a", tree_a, Some(manifest_a));
    provider.add_source("dep", tree_dep, None);

    let config = make_config(vec![(
        "a",
        git_spec("https://example.com/a.git", Some("v1.0.0")),
    )]);

    let graph = resolve(&config, &provider, None, &default_options()).unwrap();
    assert_eq!(
        graph.filters.get(&SourceName::from("dep")),
        Some(&vec![FilterMode::Include {
            agents: vec![],
            skills: vec!["frontend-design".into()],
        }])
    );
}

#[test]
fn direct_and_transitive_filters_are_both_collected_for_same_source() {
    let dir = TempDir::new().unwrap();
    let tree_a = dir.path().join("a");
    let tree_dep = dir.path().join("dep");
    std::fs::create_dir_all(&tree_a).unwrap();
    std::fs::create_dir_all(&tree_dep).unwrap();

    let manifest_a = make_manifest_with_filters(
        "a",
        "1.0.0",
        vec![(
            "dep",
            "https://example.com/dep.git",
            ">=1.0.0",
            FilterConfig {
                skills: Some(vec!["skill-b".into(), "skill-c".into()]),
                ..FilterConfig::default()
            },
        )],
    );

    let mut provider = MockProvider::new();
    provider.add_versions("https://example.com/a.git", vec![(1, 0, 0)]);
    provider.add_versions("https://example.com/dep.git", vec![(1, 0, 0)]);
    provider.add_source("a", tree_a, Some(manifest_a));
    provider.add_source("dep", tree_dep, None);

    let mut dependencies = IndexMap::new();
    dependencies.insert(
        SourceName::from("a"),
        EffectiveDependency {
            name: "a".into(),
            id: SourceId::git(SourceUrl::from("https://example.com/a.git")),
            spec: git_spec("https://example.com/a.git", Some("v1.0.0")),
            subpath: None,
            filter: FilterMode::All,
            rename: RenameMap::new(),
            is_overridden: false,
            original_git: None,
        },
    );
    dependencies.insert(
        SourceName::from("dep"),
        EffectiveDependency {
            name: "dep".into(),
            id: SourceId::git(SourceUrl::from("https://example.com/dep.git")),
            spec: git_spec("https://example.com/dep.git", Some("v1.0.0")),
            subpath: None,
            filter: FilterMode::Include {
                agents: vec![],
                skills: vec!["skill-a".into(), "skill-b".into()],
            },
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
    let filters = graph.filters.get(&SourceName::from("dep")).unwrap();
    assert_eq!(filters.len(), 2);
    assert!(filters.contains(&FilterMode::Include {
        agents: vec![],
        skills: vec!["skill-a".into(), "skill-b".into()],
    }));
    assert!(filters.contains(&FilterMode::Include {
        agents: vec![],
        skills: vec!["skill-b".into(), "skill-c".into()],
    }));
}

#[test]
fn filtered_parent_dep_does_not_seed_unfiltered_child_items() {
    let dir = TempDir::new().unwrap();
    let parent_tree = dir.path().join("parent");
    let child_tree = dir.path().join("child");
    std::fs::create_dir_all(&parent_tree).unwrap();
    std::fs::create_dir_all(&child_tree).unwrap();
    write_minimal_package_marker(&parent_tree);
    write_minimal_package_marker(&child_tree);
    write_agent(&parent_tree, "runner", &[]);
    write_agent(&child_tree, "danger", &["missing-skill"]);

    let parent_manifest = make_manifest(
        "parent",
        "1.0.0",
        vec![("child", "https://example.com/child.git", "v1.0.0")],
    );

    let mut provider = MockProvider::new();
    provider.add_versions("https://example.com/parent.git", vec![(1, 0, 0)]);
    provider.add_versions("https://example.com/child.git", vec![(1, 0, 0)]);
    provider.add_source("parent", parent_tree, Some(parent_manifest));
    provider.add_source("child", child_tree, None);

    let mut dependencies = IndexMap::new();
    dependencies.insert(
        SourceName::from("parent"),
        EffectiveDependency {
            name: "parent".into(),
            id: SourceId::git(SourceUrl::from("https://example.com/parent.git")),
            spec: git_spec("https://example.com/parent.git", Some("v1.0.0")),
            subpath: None,
            filter: FilterMode::Include {
                agents: vec!["runner".into()],
                skills: vec![],
            },
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
    assert!(graph.nodes.contains_key("parent"));
    assert!(graph.nodes.contains_key("child"));
}
