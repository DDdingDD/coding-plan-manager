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
    tauri::Builder::default()
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let db_path = dir.join("cpm.db");
            let conn = db::init_db(db_path.to_string_lossy().as_ref()).map_err(|e| e.to_string())?;
            app.manage(state::AppState {
                db: Arc::new(Mutex::new(conn)),
                servers: Arc::new(Mutex::new(std::collections::HashMap::new())),
            });
            tray::setup_tray(app)?;
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
            commands::aggregators::start_aggregator,
            commands::aggregators::stop_aggregator,
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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
