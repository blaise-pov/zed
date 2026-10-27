use std::{path::Path, sync::Arc};

use crate::init_test;
use fs::FakeFs;
use project::{
    Project, ProjectEntryId, project_search::PathInclusionMatcher,
    search::{SearchQuery, SearchResult},
};
use serde_json::json;
use settings::Settings;
use util::{
    path,
    paths::{PathMatcher, PathStyle},
    rel_path::RelPath,
};
use worktree::{Entry, EntryKind, WorktreeSettings};

#[gpui::test]
async fn test_path_inclusion_matcher(cx: &mut gpui::TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.background_executor.clone());
    fs.insert_tree(
        "/root",
        json!({
            ".gitignore": "src/data/\n",
            "src": {
                "data": {
                    "main.csv": "field_1,field_2,field_3",
                },
                "lib": {
                    "main.txt": "Are you familiar with fields?",
                },
            },
        }),
    )
    .await;

    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    let worktree = project.update(cx, |project, cx| project.worktrees(cx).next().unwrap());
    let (worktree_settings, worktree_snapshot) = worktree.update(cx, |worktree, cx| {
        let settings_location = worktree.settings_location(cx);
        return (
            WorktreeSettings::get(Some(settings_location), cx).clone(),
            worktree.snapshot(),
        );
    });

    // Manually create a test entry for the gitignored directory since it won't
    // be loaded by the worktree
    let entry = Entry {
        id: ProjectEntryId::from_proto(1),
        kind: EntryKind::UnloadedDir,
        path: Arc::from(RelPath::from_unix_str(Path::new("src/data")).unwrap()),
        inode: 0,
        mtime: None,
        canonical_path: None,
        is_ignored: true,
        is_hidden: false,
        is_always_included: false,
        is_external: false,
        is_private: false,
        size: 0,
        char_bag: Default::default(),
        is_fifo: false,
    };

    // 1. Test searching for `field`, including ignored files without any
    // inclusion and exclusion filters.
    let include_ignored = true;
    let files_to_include = PathMatcher::default();
    let files_to_exclude = PathMatcher::default();
    let match_full_paths = false;
    let search_query = SearchQuery::text(
        "field",
        false,
        false,
        include_ignored,
        files_to_include,
        files_to_exclude,
        match_full_paths,
        None,
    )
    .unwrap();

    let path_matcher = PathInclusionMatcher::new(Arc::new(search_query));
    assert!(path_matcher.should_scan_gitignored_dir(
        &entry,
        &worktree_snapshot,
        &worktree_settings
    ));

    // 2. Test searching for `field`, including ignored files but updating
    // `files_to_include` to only include files under `src/lib`.
    let include_ignored = true;
    let files_to_include = PathMatcher::new(vec!["src/lib"], PathStyle::Unix).unwrap();
    let files_to_exclude = PathMatcher::default();
    let match_full_paths = false;
    let search_query = SearchQuery::text(
        "field",
        false,
        false,
        include_ignored,
        files_to_include,
        files_to_exclude,
        match_full_paths,
        None,
    )
    .unwrap();

    let path_matcher = PathInclusionMatcher::new(Arc::new(search_query));
    assert!(!path_matcher.should_scan_gitignored_dir(
        &entry,
        &worktree_snapshot,
        &worktree_settings
    ));
}

#[gpui::test]
async fn test_search_in_worktree(cx: &mut gpui::TestAppContext) {
    init_test(cx);

    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/visible-worktree"),
        json!({
            "one.rs": "const TARGET: &str = \"target_visible\";",
        }),
    )
    .await;
    fs.insert_tree(
        path!("/invisible-worktree"),
        json!({
            "two.rs": "const TARGET: &str = \"target_invisible\";",
        }),
    )
    .await;

    let project = Project::test(fs.clone(), [path!("/visible-worktree").as_ref()], cx).await;
    let (invisible_worktree, _) = project
        .update(cx, |p, cx| p.find_or_create_worktree(path!("/invisible-worktree"), false, cx))
        .await
        .unwrap();
    let invisible_id = invisible_worktree.read_with(cx, |w, _| w.id());
    let _ = invisible_worktree
        .read_with(cx, |w, _| w.as_local().unwrap().scan_complete())
        .await;

    let visible_worktree = project.read_with(cx, |p, cx| p.visible_worktrees(cx).next().unwrap());
    let visible_id = visible_worktree.read_with(cx, |w, _| w.id());

    // 1. Regular project.search() only finds visible worktree
    let query = SearchQuery::text(
        "TARGET",
        false,
        true,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    )
    .unwrap();
    let rx = project.update(cx, |p, cx| p.search(query.clone(), cx));
    let mut results = Vec::new();
    while let Ok(res) = rx.rx.recv().await {
        if let SearchResult::Buffer { buffer, .. } = res {
            results.push(buffer.read_with(cx, |b, cx| {
                b.file().unwrap().full_path(cx).to_string_lossy().to_string()
            }));
        }
    }
    assert_eq!(results.len(), 1);
    assert!(results[0].contains("one.rs"));

    // 2. search_in_worktree() for invisible worktree returns only invisible worktree
    let rx = project.update(cx, |p, cx| p.search_in_worktree(query.clone(), invisible_id, cx));
    let mut results = Vec::new();
    while let Ok(res) = rx.rx.recv().await {
        if let SearchResult::Buffer { buffer, .. } = res {
            results.push(buffer.read_with(cx, |b, cx| {
                b.file().unwrap().full_path(cx).to_string_lossy().to_string()
            }));
        }
    }
    assert_eq!(results.len(), 1);
    assert!(results[0].contains("two.rs"));

    // 3. search_in_worktree() for visible worktree returns only visible worktree
    let rx = project.update(cx, |p, cx| p.search_in_worktree(query.clone(), visible_id, cx));
    let mut results = Vec::new();
    while let Ok(res) = rx.rx.recv().await {
        if let SearchResult::Buffer { buffer, .. } = res {
            results.push(buffer.read_with(cx, |b, cx| {
                b.file().unwrap().full_path(cx).to_string_lossy().to_string()
            }));
        }
    }
    assert_eq!(results.len(), 1);
    assert!(results[0].contains("one.rs"));
}
