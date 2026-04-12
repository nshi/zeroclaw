//! Mobile entry point for Mentat Desktop (iOS/Android).

#[tauri::mobile_entry_point]
fn main() {
    mentat_desktop::run();
}
