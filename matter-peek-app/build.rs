use tauri_build::{Attributes, DefaultPermissionRule, InlinedPlugin};

fn main() {
    // The commands of the app's own plugins, which the page calls; the
    // capability allows them. Google Play services, on Android:
    let play = InlinedPlugin::new()
        .commands(&[
            "available",
            "watch_shares",
            "commission",
            "handed_over",
            "share",
        ])
        .default_permission(DefaultPermissionRule::AllowAllCommands);
    // The window's colours, light or dark, as the page's:
    let theme = InlinedPlugin::new()
        .commands(&["set_theme"])
        .default_permission(DefaultPermissionRule::AllowAllCommands);

    tauri_build::try_build(
        Attributes::new()
            .plugin("play", play)
            .plugin("theme", theme),
    )
    .expect("the Tauri build script failed");
}
