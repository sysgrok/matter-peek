use tauri_build::{Attributes, DefaultPermissionRule, InlinedPlugin};

fn main() {
    // The commands of the app's Android plugin (Google Play services), which
    // the page calls; the capability allows them.
    let play = InlinedPlugin::new()
        .commands(&["available", "watch_shares", "commission", "handed_over"])
        .default_permission(DefaultPermissionRule::AllowAllCommands);

    tauri_build::try_build(Attributes::new().plugin("play", play))
        .expect("the Tauri build script failed");
}
