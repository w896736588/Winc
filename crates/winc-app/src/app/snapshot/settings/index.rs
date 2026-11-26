//! 设置中「符号索引」一组的测试：选择索引的语言，以及是否跳过依赖目录和构建输出目录。

use super::*;

#[test]
fn the_index_settings_choose_languages_and_folder_skips() {
    let dir = sample_folder("settings-index");
    let mut app = app_with(&dir.join("short.rs"), false);
    app.open_settings();
    app.settings_show_page(SettingsPage::Editor);
    app.settings.motion = None;
    paint(&mut app);

    // 默认收录全部语言，索引时跳过依赖目录和构建输出目录。
    assert_eq!(app.choice_text(Setting::IndexLanguages).0, "全部");
    assert!(app.switch_on(Setting::IndexIgnoreDependencies));
    assert!(app.switch_on(Setting::IndexIgnoreBuild));

    // 一行在页面下方时用键盘选中并切换。
    app.settings.selected = Setting::IndexIgnoreBuild;
    app.settings_activate();
    assert_eq!(app.config.index_ignore_build, Some(false));
    app.settings.selected = Setting::IndexIgnoreDependencies;
    app.settings_activate();
    assert_eq!(app.config.index_ignore_dependencies, Some(false));

    // 「索引的文件类型」：菜单列出全部语言，默认全部打勾；点击一项即不收录，再点恢复。
    app.settings.selected = Setting::IndexLanguages;
    app.settings_activate();
    let index = |id: &str| {
        winc_syntax::languages()
            .iter()
            .position(|l| l.id == id)
            .unwrap()
    };
    assert!(app.menu_checked(MenuCommand::IndexLanguage(index("go"))));
    app.run_menu_command(MenuCommand::IndexLanguage(index("go")));
    assert_eq!(app.config.index_exclude, ["go"]);
    assert!(!app.menu_checked(MenuCommand::IndexLanguage(index("go"))));
    assert_eq!(app.choice_text(Setting::IndexLanguages).0, "Go");

    app.settings_open_menu(Setting::IndexLanguages);
    app.run_menu_command(MenuCommand::IndexLanguage(index("go")));
    assert!(app.config.index_exclude.is_empty());
    assert_eq!(app.choice_text(Setting::IndexLanguages).0, "全部");

    std::fs::remove_dir_all(&dir).unwrap();
}
