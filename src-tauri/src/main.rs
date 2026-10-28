// 发布版不弹控制台窗口（Tauri 约定）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    codemaker_hub_light_lib::run()
}
