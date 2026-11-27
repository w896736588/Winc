//! Git 更改列表的跨平台显示模式测试，不依赖外部 Git 进程。

use super::*;
use crate::app::input::context_menu::MenuEntry;
use crate::app::{CommitRow, CommitTarget, OtherGroup, OtherRow, PopupRow};
use winc_editor::git::{Change, Entry, Head, Status};

#[test]
fn git_layout_menu_rebuilds_and_draws_the_current_repository_tree() {
    let dir = sample_folder("git-layout-menu");
    let mut app = app_with(&dir, false);
    let root = app.editor.root().unwrap().to_path_buf();
    app.git.root = Some(root.clone());
    app.git.status = Some(Status {
        head: Some(Head::Branch("main".into())),
        entries: ["src/main.rs", "src/nested/lib.rs", "Cargo.toml"]
            .into_iter()
            .map(|path| Entry {
                path: root.join(path),
                from: None,
                staged: None,
                unstaged: Some(Change::Modified),
                is_dir: false,
            })
            .collect(),
        ..Status::default()
    });
    app.panels.sidebar.view = SidebarView::Commit;
    paint(&mut app);
    assert!(
        !app.commit_rows()
            .iter()
            .any(|r| matches!(r, CommitRow::Dir { .. }))
    );

    for command in [MenuCommand::CommitTree, MenuCommand::CommitList] {
        // 模式切换不能继续绘制展开动画里保存的旧版列表。
        app.git.commit.motion = Some(crate::app::git::commit::CommitMotion {
            rows: app.commit_rows(),
            parent: 0,
            count: 3,
            expanding: true,
            clock: crate::app::Clock::stopped(
                std::time::Duration::from_secs(1),
                std::time::Duration::from_millis(500),
            ),
        });
        app.press_commit(CommitTarget::More);
        paint(&mut app);
        let menu = app.overlays.context_menu.as_ref().unwrap();
        let top = &app.popup().unwrap().levels[0];
        let index = top
            .rows
            .iter()
            .position(|row| match row {
                PopupRow::Submenu { rows, .. } => rows.iter().any(|row| {
                    matches!(row, PopupRow::Item { tag, .. }
                    if MenuEntry::command_at(&menu.entries, *tag) == Some(command))
                }),
                _ => false,
            })
            .unwrap();
        let at = top.row_rects[index];
        click(&mut app, (at.x + 20.0, at.y + at.h / 2.0));
        paint(&mut app);
        let menu = app.overlays.context_menu.as_ref().unwrap();
        let sub = &app.popup().unwrap().levels[1];
        let index = sub
            .rows
            .iter()
            .position(|row| {
                matches!(row, PopupRow::Item { tag, .. }
                if MenuEntry::command_at(&menu.entries, *tag) == Some(command))
            })
            .unwrap();
        let at = sub.row_rects[index];
        click(&mut app, (at.x + 20.0, at.y + at.h / 2.0));
        paint(&mut app);
        let tree = command == MenuCommand::CommitTree;
        assert_eq!(app.state.commit_tree, tree);
        assert!(app.menu_checked(command));
        assert!(app.git.commit.motion.is_none());
        assert!(app.git.commit.fold_motion.is_none());
        assert_eq!(
            app.commit_rows()
                .iter()
                .any(|r| matches!(r, CommitRow::Dir { .. })),
            tree
        );
        assert_eq!(
            app.hits
                .commit
                .iter()
                .any(|(_, t)| matches!(t, CommitTarget::Dir(_))),
            tree
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn git_layout_tree_applies_to_other_repositories() {
    let folder = sample_folder("git-layout-others");
    let mut app = app_with(&folder, false);
    let root = app.editor.root().unwrap().to_path_buf();
    let other = root.join("other");
    app.git.root = Some(root.clone());
    app.git.repositories = vec![root.clone(), other.clone()];
    app.git.status = Some(Status::default());
    app.state.commit_all_repositories = Some(true);
    app.git.summary_for_test(
        other.clone(),
        Status {
            entries: ["src/main.rs", "src/nested/lib.rs", "Cargo.toml"]
                .into_iter()
                .map(|path| Entry {
                    path: other.join(path),
                    from: None,
                    staged: None,
                    unstaged: Some(Change::Modified),
                    is_dir: false,
                })
                .collect(),
            ..Status::default()
        },
    );
    let flat = app.commit_other_rows();
    app.toggle_commit_layout();
    let tree = app.commit_other_rows();
    assert!(tree.len() > flat.len(), "树形模式应显示其他仓库的目录行");
    assert!(
        tree.iter()
            .any(|r| matches!(r, OtherRow::TreeFile(1, 1, OtherGroup::Changes, 2)))
    );
    app.panels.sidebar.view = SidebarView::Commit;
    paint(&mut app);
    if std::env::var_os("WINC_SNAPSHOT_DIR").is_some() {
        shoot(&mut app, "git-layout-other-tree");
    }
    let dir = tree
        .iter()
        .position(|r| matches!(r, OtherRow::Dir(_, _, path, _, 0) if path == Path::new("src")))
        .unwrap();
    let hit = app
        .hits
        .commit
        .iter()
        .find(|(_, t)| *t == CommitTarget::OtherDir(dir))
        .unwrap()
        .0;
    click(&mut app, (hit.x + 80.0, hit.y + hit.h / 2.0));
    assert_eq!(app.git.root, Some(root));
    assert!(
        !app.commit_other_rows()
            .iter()
            .any(|r| matches!(r, OtherRow::TreeFile(1, 0 | 1, ..)))
    );
    assert!(
        app.commit_other_rows()
            .iter()
            .any(|r| matches!(r, OtherRow::TreeFile(1, 2, ..)))
    );
    app.run_commit_more_command(MenuCommand::CommitExpandAll);
    assert_eq!(app.commit_other_rows(), tree);
    app.toggle_commit_layout();
    assert_eq!(app.commit_other_rows(), flat);

    // 只暂存了一部分的文件分别出现在两组，目录的折叠互不影响。
    app.git.summary_for_test(
        other.clone(),
        Status {
            entries: vec![Entry {
                path: other.join("src/main.rs"),
                from: None,
                staged: Some(Change::Modified),
                unstaged: Some(Change::Modified),
                is_dir: false,
            }],
            ..Status::default()
        },
    );
    app.git.commit.set_staging(true);
    app.toggle_commit_layout();
    let rows = app.commit_other_rows();
    for group in [OtherGroup::Staged, OtherGroup::Changes] {
        assert!(
            rows.iter()
                .any(|r| matches!(r, OtherRow::TreeFile(1, 0, g, 1) if *g == group))
        );
    }
    let dir = rows
        .iter()
        .position(|r| matches!(r, OtherRow::Dir(_, OtherGroup::Staged, ..)))
        .unwrap();
    app.press_commit(CommitTarget::OtherDir(dir));
    let rows = app.commit_other_rows();
    assert!(
        !rows
            .iter()
            .any(|r| matches!(r, OtherRow::TreeFile(_, _, OtherGroup::Staged, _)))
    );
    assert!(
        rows.iter()
            .any(|r| matches!(r, OtherRow::TreeFile(_, _, OtherGroup::Changes, _)))
    );
    let _ = std::fs::remove_dir_all(&folder);
}

/// 暂存区视图按暂存区和工作区分组；切换视图只改变分组，不改变仓库状态。
#[test]
fn staging_groups_partition_index_and_worktree_changes() {
    use crate::app::CommitGroup::{Changes, Conflicts, Staged, Untracked};

    let folder = sample_folder("git-staging-groups");
    let mut app = app_with(&folder, false);
    let root = app.editor.root().unwrap().to_path_buf();
    let status = Status {
        entries: [
            ("src/added.rs", Some(Change::Added), None),
            ("src/modified.rs", None, Some(Change::Modified)),
            (
                "src/partial.rs",
                Some(Change::Modified),
                Some(Change::Modified),
            ),
            ("src/untracked.rs", None, Some(Change::Untracked)),
            ("src/conflict.rs", None, Some(Change::Conflicted)),
        ]
        .into_iter()
        .map(|(path, staged, unstaged)| Entry {
            path: root.join(path),
            from: None,
            staged,
            unstaged,
            is_dir: false,
        })
        .collect(),
        ..Status::default()
    };
    app.git.root = Some(root);
    app.git.status = Some(status.clone());
    app.settings_toggle(Setting::GitStagingArea);

    let rows = app.commit_rows();
    assert_eq!(rows.group(Staged), [0, 2]);
    assert_eq!(rows.group(Changes), [1, 2, 3]);
    assert_eq!(rows.group(Conflicts), [4]);
    assert!(rows.group(Untracked).is_empty());

    // 显式切回勾选式会重新分组，但不能改动暂存区或工作区的状态。
    app.settings_toggle(Setting::GitStagingArea);
    let rows = app.commit_rows();
    assert!(rows.group(Staged).is_empty());
    assert_eq!(rows.group(Changes), [0, 1, 2]);
    assert_eq!(rows.group(Untracked), [3]);
    assert_eq!(app.git.status.as_ref(), Some(&status));

    std::fs::remove_dir_all(&folder).unwrap();
}

/// 分支菜单里的「复制分支名」把分支名写入剪贴板；远程分支带着远程名。
#[test]
fn the_branch_menu_copies_the_name_to_the_clipboard() {
    use crate::app::git::branch::BranchOp;
    use winc_editor::git::Branch;

    let dir = sample_folder("git-copy-branch-name");
    let mut app = app_with(&dir, false);
    let branch = Branch {
        name: "origin/feat".to_owned(),
        remote: true,
        current: false,
        upstream: None,
        gone: false,
        oid: String::new(),
        time: 0,
        local: String::new(),
        ahead: 0,
        behind: 0,
    };
    app.run_branch_op(BranchOp::CopyName, branch);
    assert_eq!(app.clipboard_text().as_deref(), Some("origin/feat"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 基于远程分支签出的新分支带着别的分支的上游：推送、同步都发布到远程的同名分支，不更新
/// 上游那个分支；上游本来就与分支同名时照旧推送。
#[test]
fn a_branch_that_tracks_another_branch_publishes_to_its_own_name() {
    use crate::app::SyncState;
    use crate::app::git::commit::PushIntent;
    use winc_editor::git::Branch;

    let dir = sample_folder("git-push-same-name");
    let mut app = app_with(&dir, false);
    let root = app.editor.root().unwrap().to_path_buf();
    app.git.root = Some(root);
    app.git.remotes = vec!["origin".to_owned()];
    app.git.status = Some(Status {
        head: Some(Head::Branch("feat".into())),
        ..Status::default()
    });
    let branch = |name: &str, remote: bool, upstream: Option<&str>| Branch {
        name: name.to_owned(),
        remote,
        current: !remote,
        upstream: upstream.map(str::to_owned),
        gone: false,
        oid: String::new(),
        time: 0,
        local: String::new(),
        ahead: 0,
        behind: 0,
    };
    app.git.branches = vec![
        branch("feat", false, Some("origin/main")),
        branch("origin/main", true, None),
    ];
    assert!(!app.push_tracks_same_name());
    assert_eq!(app.git_sync_state(), SyncState::Publish);
    assert_eq!(
        app.push_intent(),
        Some(PushIntent::Publish("origin".to_owned()))
    );

    // 上游与当前分支同名：照旧推送到上游。
    app.git.branches[0].upstream = Some("origin/feat".to_owned());
    assert!(app.push_tracks_same_name());
    assert_eq!(app.push_intent(), Some(PushIntent::Push));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 提交视图标题行在「Git 日志」左边显示拉取、获取、推送三个按钮；日志的分支面板顶部是分支名
/// 搜索框，搜索实时过滤分支和标签。
#[test]
fn the_commit_header_has_sync_buttons_and_the_branch_search_filters_the_list() {
    use crate::app::git::branch::picker::BranchRow;
    use crate::app::git::log::BRANCHES_SEARCH_H;
    use winc_editor::git::Branch;

    let dir = sample_folder("git-log-branch-search");
    let mut app = app_with(&dir, false);
    let branch = |name: &str, remote: bool, current: bool| Branch {
        name: name.to_owned(),
        remote,
        current,
        upstream: None,
        gone: false,
        oid: String::new(),
        time: 0,
        local: String::new(),
        ahead: 0,
        behind: 0,
    };
    app.git.branches = vec![
        branch("main", false, true),
        branch("feature/login", false, false),
        branch("origin/main", true, false),
    ];
    app.git.tags = vec!["v1.0".to_owned()];

    // 拉取、获取、推送三个按钮在「Git 日志」按钮左边，互相之间不重叠；搜索框在分支面板顶部。
    let parts = app.commit_parts();
    let sync = parts.sync.expect("默认宽度下应显示拉取、获取、推送按钮");
    assert!(sync[0].right() <= sync[1].x);
    assert!(sync[1].right() <= sync[2].x);
    assert!(sync[2].right() < parts.tools[0].0.x);
    assert_eq!(app.log_parts().branches_search().h, BRANCHES_SEARCH_H);

    // 侧栏收窄到最小宽度时不画同步按钮，把宽度留给项目名。
    app.panels.sidebar.width = crate::theme::MIN_PANEL_WIDTH;
    paint(&mut app);
    assert!(app.commit_parts().sync.is_none());
    app.panels.sidebar.width = crate::theme::DEFAULT_PANEL_WIDTH;
    paint(&mut app);

    // 不搜索时最上面是「全部分支」。
    assert_eq!(app.log_branch_rows().first(), Some(&BranchRow::All));

    let branch_names = |app: &App| -> Vec<String> {
        app.log_branch_rows()
            .into_iter()
            .filter_map(|row| match row {
                BranchRow::Branch { index, .. } => {
                    app.git.branches.get(index).map(|b| b.name.clone())
                }
                _ => None,
            })
            .collect()
    };
    assert!(branch_names(&app).iter().any(|name| name == "main"));

    // 输入分支名的一部分只留下命中的分支，拉取、获取、推送之外的「全部分支」不再列出。
    app.git.log.branch_search.set_text("login");
    assert_eq!(branch_names(&app), ["feature/login"]);
    assert!(!app.log_branch_rows().contains(&BranchRow::All));

    // 标签同样按名字过滤，并且搜索时展开列出。
    app.git.log.branch_search.set_text("v1.0");
    assert!(branch_names(&app).is_empty());
    assert!(
        app.log_branch_rows()
            .iter()
            .any(|row| matches!(row, BranchRow::Tag(_)))
    );

    // 没有命中时列表为空，清空后恢复「全部分支」。
    app.git.log.branch_search.set_text("no-such-branch");
    assert!(app.log_branch_rows().is_empty());
    app.git.log.branch_search.set_text("");
    assert_eq!(app.log_branch_rows().first(), Some(&BranchRow::All));
    let _ = std::fs::remove_dir_all(&dir);
}
