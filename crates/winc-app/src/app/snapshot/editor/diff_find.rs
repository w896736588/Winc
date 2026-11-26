//! 可编辑差异中的查找：光标停在左侧（旧版本）时查左侧的只读快照，在右侧时查工作区文件。

use winc_editor::diff_view::{DiffSide, DiffView, LEFT, RIGHT, ReadSelection};

use crate::keymap::Action;

use super::*;

/// 打开对应工作区文件 `work.rs` 的可编辑差异：左侧是 `HEAD`，右侧是工作区的文件。
fn editable_diff(name: &str) -> (App, PathBuf) {
    let dir = sample_folder(name);
    let path = dir.join("work.rs");
    std::fs::write(&path, "one two\nRIGHT\nfour\nfive\n").unwrap();
    let mut app = app_with(&dir, false);
    let language = winc_syntax::detect("work.rs");
    let view = DiffView::new(
        DiffSide::new("HEAD", "one two\nleft only\nfour\n", None, language),
        DiffSide::new(
            "工作区",
            "one two\nRIGHT\nfour\nfive\n",
            Some(path.clone()),
            language,
        ),
    );
    app.editor.show_diff_in(None, "work.rs", view);
    app.focus = Focus::Editor;
    paint(&mut app);
    (app, path)
}

/// 像鼠标那样把只读一侧的光标放到第 `side` 侧的 `range`，并把光标从右侧移开。
fn point(app: &mut App, side: usize, range: (usize, usize)) {
    let view = app
        .editor
        .active_mut()
        .unwrap()
        .diff
        .as_deref_mut()
        .unwrap();
    view.editing = false;
    view.read_selection = Some(ReadSelection { side, range });
}

/// 光标停在左侧时 ⌘F 查左侧的旧版本：跳到左侧的匹配，不切换到右侧的文件；左侧只读，
/// 替换快捷键只打开查找。光标在右侧时照旧查工作区文件。
#[test]
fn find_in_an_editable_diff_stays_on_the_caret_side() {
    let (mut app, path) = editable_diff("diff-find-side");
    let diff = app.editor.active().unwrap().id;

    // 左侧：查找在差异标签中进行，匹配是左侧的选区。
    point(&mut app, LEFT, (8, 8));
    assert!(app.run_action(Action::Find));
    assert!(app.bars.find.open);
    assert_eq!(app.focus, Focus::Search);
    app.bars.find.input.set_text("left only");
    app.bars.search.query = "left only".into();
    app.search_changed();
    paint(&mut app);
    assert_eq!(
        app.editor.active().unwrap().id,
        diff,
        "查找没有切到右侧的文件"
    );
    let view = app.editor.active().unwrap().diff.as_deref().unwrap();
    let read = view.read_selection.unwrap();
    assert_eq!(read.side, LEFT);
    assert_eq!(
        view.side(LEFT)
            .text
            .slice(read.range.0..read.range.1)
            .to_string(),
        "left only"
    );
    // 查找条出现在左侧窗口里，不再浮在右侧。
    let parts = crate::app::diff_parts(app.layout.editor, view, app.metrics.cell);
    let pane = parts.panes[LEFT].text;
    let bar = app.layout.search;
    assert!(
        bar.w > 0.0 && bar.x >= pane.x && bar.right() <= pane.right(),
        "查找条应在左侧窗口内：{bar:?}，左侧文字区 {pane:?}"
    );
    app.close_search();

    // 左侧是旧版本：替换快捷键同样只打开查找。
    point(&mut app, LEFT, (0, 0));
    app.open_search(true);
    assert!(app.bars.find.open && !app.bars.find.replace_open);
    app.close_search();

    // 右侧：查找在工作区文件中进行，匹配是它的选区。
    let view = app
        .editor
        .active_mut()
        .unwrap()
        .diff
        .as_deref_mut()
        .unwrap();
    view.editing = true;
    view.read_selection = None;
    assert!(app.run_action(Action::Find));
    app.bars.find.input.set_text("RIGHT");
    app.bars.search.query = "RIGHT".into();
    app.search_changed();
    paint(&mut app);
    let source = app
        .editor
        .documents
        .iter()
        .find(|d| d.path.as_deref() == Some(path.as_path()))
        .unwrap();
    assert_eq!(
        source
            .text_range(source.selection.from(), source.selection.to())
            .as_deref(),
        Some("RIGHT")
    );
    // 右侧查找时查找条回到右侧。
    let view = app.editor.active().unwrap().diff.as_deref().unwrap();
    let parts = crate::app::diff_parts(app.layout.editor, view, app.metrics.cell);
    assert!(app.layout.search.x > parts.panes[LEFT].text.right());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// 只读差异同样把查找条放在所查的一侧。
#[test]
fn find_in_a_readonly_diff_puts_the_bar_on_the_searched_side() {
    let dir = sample_folder("diff-find-side-readonly");
    let mut app = app_with(&dir, false);
    let language = winc_syntax::detect("main.rs");
    let view = DiffView::new(
        DiffSide::new("abc1234", "one\ntwo\n", None, language),
        DiffSide::new("worktree", "one\nTWO\n", None, language),
    );
    app.editor.show_diff_in(None, "main.rs", view);
    app.focus = Focus::Editor;
    paint(&mut app);
    assert!(app.run_action(Action::Find));
    point(&mut app, LEFT, (0, 0));
    app.bars.find.input.set_text("two");
    app.bars.search.query = "two".into();
    app.search_changed();
    paint(&mut app);
    let in_left = |app: &App| {
        let view = app.editor.active().unwrap().diff.as_deref().unwrap();
        let parts = crate::app::diff_parts(app.layout.editor, view, app.metrics.cell);
        let pane = parts.panes[LEFT].text;
        let bar = app.layout.search;
        bar.w > 0.0 && bar.x >= pane.x && bar.right() <= pane.right()
    };
    assert!(in_left(&app), "查找左侧时查找条在左侧窗口里");

    // 光标移到右侧：查找右侧，查找条回到右侧。
    point(&mut app, RIGHT, (0, 0));
    app.search_changed();
    paint(&mut app);
    assert!(!in_left(&app), "查找右侧时查找条不在左侧窗口里");
    let _ = std::fs::remove_dir_all(&dir);
}
