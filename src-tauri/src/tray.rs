use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    App, AppHandle, Manager,
};

/// 菜单项标识：显示主窗口
const TRAY_ID_SHOW: &str = "tray_show";
/// 菜单项标识：退出程序
const TRAY_ID_QUIT: &str = "tray_quit";

/// 创建系统托盘图标与菜单，在 app setup 阶段调用一次
pub fn setup_tray(app: &App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, TRAY_ID_SHOW, "显示主窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, TRAY_ID_QUIT, "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    let icon = app
        .default_window_icon()
        .expect("打包配置已提供窗口图标，托盘图标可直接复用")
        .clone();

    TrayIconBuilder::with_id("main-tray")
        .icon(icon)
        .tooltip("Coding Plan Manager")
        .menu(&menu)
        // Windows 惯例：左键单击显示窗口，右键才弹菜单
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            TRAY_ID_SHOW => show_main_window(app),
            TRAY_ID_QUIT => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// 取回主窗口并显示、置前（托盘菜单/左键单击共用）
fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}
