//! 保存的远端连接（SFTP 配置）的测试：列在主机面板中、复制新增、保存时的校验和移除。

use super::*;
use crate::app::git::form::FormKind;
use winc_config::Remote;
use winit::keyboard::{Key, NamedKey};

/// 主机面板中各行的标题和说明。
fn rows(app: &App) -> Vec<(String, String)> {
    let list = app.overlays.picker.remote.as_ref().unwrap();
    (app.overlays.picker.hits.iter())
        .map(|&row| {
            let row = &list.rows[row];
            (row.title.clone(), row.detail.clone())
        })
        .collect()
}

#[test]
fn a_saved_connection_is_copied_from_the_picker_and_removed() {
    let mut app = blank_app(false);
    app.state.remotes = vec![
        Remote {
            target: "me@devbox:2200".into(),
            directory: "/srv/app".into(),
        },
        Remote {
            target: "deploy@10.0.0.2".into(),
            directory: "~/www".into(),
        },
    ];
    app.open_remote_picker();
    // 保存的连接排在「新主机…」之后，与 `ssh` 配置和最近目录分开列出。
    assert_eq!(rows(&app)[0].0, "新主机…");
    assert_eq!(
        rows(&app)[1],
        ("me@devbox:2200".to_owned(), "/srv/app".to_owned())
    );
    assert_eq!(
        rows(&app)[2],
        ("deploy@10.0.0.2".to_owned(), "~/www".to_owned())
    );
    assert!(
        rows(&app)
            .iter()
            .any(|(title, _)| title == "新增 SFTP 连接…")
    );

    // `⇥` 复制选中的连接：对话框里填好地址、账号和目录，改好后保存为新的一条。
    app.picker_move(true);
    assert!(app.remote_picker_key(&Key::Named(NamedKey::Tab)));
    assert!(!app.overlays.picker.open);
    let dialog = app.overlays.form_dialog.as_ref().unwrap();
    assert_eq!(dialog.kind, FormKind::SavedRemote { copy: true });
    assert_eq!(dialog.name.text(), "devbox:2200");
    assert_eq!(dialog.second.text(), "me");
    assert_eq!(dialog.third.text(), "");
    assert_eq!(dialog.fifth.text(), "/srv/app");
    app.overlays
        .form_dialog
        .as_mut()
        .unwrap()
        .fifth
        .set_text("/srv/app2");
    app.submit_form_dialog();
    assert_eq!(
        app.state.remotes[0],
        Remote {
            target: "me@devbox:2200".into(),
            directory: "/srv/app2".into(),
        }
    );
    assert_eq!(app.state.remotes.len(), 3);
    // 保存后回到主机面板，新的连接在第一行。
    assert!(app.overlays.picker.open);
    assert_eq!(
        rows(&app)[1],
        ("me@devbox:2200".to_owned(), "/srv/app2".to_owned())
    );

    // 行尾的「✕」只去掉这一条保存的连接。
    app.forget_remote_row(1);
    assert_eq!(app.state.remotes.len(), 2);
    assert!(
        app.state
            .remotes
            .iter()
            .all(|remote| remote.directory != "/srv/app2")
    );
}

#[test]
fn the_save_dialog_validates_the_address_and_keeps_the_password() {
    let mut app = blank_app(false);
    app.open_saved_dialog("bad host", "/srv", false);
    let dialog = app.overlays.form_dialog.as_ref().unwrap();
    assert_eq!(dialog.kind, FormKind::SavedRemote { copy: false });
    assert_eq!(dialog.fifth.text(), "/srv");
    // 地址无效时不能保存。
    let plan = app.form_plan(dialog);
    assert!(!plan.enabled && plan.problem.is_some());

    // 账号和密码：密码存进系统的凭据存储（测试中在进程内）。
    let dialog = app.overlays.form_dialog.as_mut().unwrap();
    dialog.name.set_text("host.example");
    dialog.second.set_text("me");
    dialog.third.set_text("secret");
    let plan = app.form_plan(app.overlays.form_dialog.as_ref().unwrap());
    assert!(plan.enabled && plan.problem.is_none());
    app.submit_form_dialog();
    assert_eq!(
        app.state.remotes,
        [Remote {
            target: "me@host.example".into(),
            directory: "/srv".into(),
        }]
    );
    let kept = crate::credentials::find("ssh://host.example", Some("me")).unwrap();
    assert_eq!(kept.password, "secret");

    // 没有账号时不能保存密码，原因写在输入框里。
    app.open_saved_dialog("host.example", "/srv", false);
    let dialog = app.overlays.form_dialog.as_mut().unwrap();
    dialog.second.set_text("");
    dialog.third.set_text("secret");
    let plan = app.form_plan(app.overlays.form_dialog.as_ref().unwrap());
    assert!(!plan.enabled && plan.problem.is_some());
    app.close_form_dialog();

    // 密钥文件必须存在。
    app.open_saved_dialog("host.example", "/srv", false);
    app.overlays
        .form_dialog
        .as_mut()
        .unwrap()
        .fourth
        .set_text("/no/such/key");
    let plan = app.form_plan(app.overlays.form_dialog.as_ref().unwrap());
    assert!(!plan.enabled && plan.problem.is_some());
}
