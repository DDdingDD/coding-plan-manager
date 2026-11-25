mod commands;
pub mod db;
pub mod proxy;
mod state;
mod token;
mod tray;

use std::sync::{Arc, Mutex};
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // release 是 panic=abort 且 GUI 子系统无控制台，任何启动失败/panic 都无声消失
    // （用户只见双击后闪退，连一个字节的报错都没有）。装 panic 钩子把 panic 信息
    // 用原生对话框显示出来。rfd 未启用 common-controls-v6 特性，此处走
    // MessageBoxW（user32），与 comctl32 v6 无关，测试二进制同样安全（坑 #1）
    std::panic::set_hook(Box::new(|info| {
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("Coding Plan Manager 崩溃")
            .set_description(format!("{info}"))
            .show();
    }));

    if let Err(e) = tauri::Builder::default()
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let db_path = dir.join("cpm.db");
            let conn =
                db::init_db(db_path.to_string_lossy().as_ref()).map_err(|e| e.to_string())?;
            app.manage(state::AppState {
                db: Arc::new(Mutex::new(conn)),
                db_path: db_path.clone(),
                servers: Arc::new(Mutex::new(std::collections::HashMap::new())),
            });
            tray::setup_tray(app)?;
            // 窗口配置为 create:false：tauri 默认在用户 setup 之前就建窗加载前端，
            // 前端首波 invoke 走 ipc:// 协议在 WebView2 线程上分发，与上面的
            // app.manage() 存在竞态（"state not managed"）。改为 AppState 就位后
            // 在此手动建窗，竞态结构性消除
            let window = app
                .config()
                .app
                .windows
                .first()
                .ok_or("tauri.conf.json 缺少窗口配置")?;
            tauri::WebviewWindowBuilder::from_config(app.handle(), window)?.build()?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::plans::list_plans,
            commands::plans::create_plan,
            commands::plans::update_plan,
            commands::plans::delete_plan,
            commands::aggregators::list_aggregators,
            commands::aggregators::create_aggregator,
            commands::aggregators::update_aggregator,
            commands::aggregators::delete_aggregator,
            commands::aggregators::set_aggregator_plans,
            commands::aggregators::reset_aggregator_usage,
            commands::aggregators::set_current_plan,
            commands::aggregators::start_aggregator,
            commands::aggregators::stop_aggregator,
            commands::config::export_config,
            commands::config::import_config,
            commands::messages::list_messages,
            commands::messages::get_message,
            commands::messages::clear_messages,
            commands::messages::global_stats,
            commands::messages::trip_stats,
            commands::messages::reset_trip,
            commands::messages::toggle_trip_pause,
            commands::messages::daily_stats,
            commands::messages::hourly_stats,
            commands::messages::model_stats,
            commands::system::get_db_info,
        ])
        .run(tauri::generate_context!())
    {
        // setup/窗口创建（含 WebView2 环境）等启动错误在此弹出：含全部 source 链，
        // 否则 tauri::Error 顶层 Display 常常不含真正的 OS 原因（如"拒绝访问"）
        rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("Coding Plan Manager 启动失败")
            .set_description(error_chain(&e))
            .show();
        std::process::exit(1);
    }
}

/// 把错误与其全部 source 链拼成可读文本（每层一行"原因: ..."）
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        msg.push_str("\n原因: ");
        msg.push_str(&s.to_string());
        source = s.source();
    }
    msg
}
