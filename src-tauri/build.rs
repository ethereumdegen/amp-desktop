fn main() {
    tauri_build::build();
    let target = tauri_utils::platform::Target::from_triple(
        &std::env::var("TARGET").expect("Cargo target is required"),
    );
    let (mut config, _) = tauri_utils::config::parse::read_from(
        target,
        &std::env::current_dir().expect("Build directory is required"),
    )
    .expect("Could not read Tauri configuration");
    if let Ok(overlay) = std::env::var("TAURI_CONFIG") {
        json_patch::merge(
            &mut config,
            &serde_json::from_str(&overlay).expect("Invalid Tauri configuration override"),
        );
    }
    let identifier = config["identifier"]
        .as_str()
        .expect("Tauri application identifier is required");
    println!("cargo:rustc-env=OMP_DESKTOP_IDENTIFIER={identifier}");
}
