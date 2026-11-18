fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_DESKTOP");
    if std::env::var_os("CARGO_FEATURE_DESKTOP").is_some() {
        tauri_build::build();
    }
}
