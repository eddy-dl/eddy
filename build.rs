use std::process::Command;

fn main() {
    // The UI itself is compiled by its own crate (ui/build.rs).
    let output = Command::new("cargo")
        .args(["pkgid", "slint"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let version = output.split('@').last().unwrap_or("Unknown");

    println!("cargo:rustc-env=DEPENDENCY_SLINT_VERSION={}", version);
    // Slint's version is in the lock file (otherwise Cargo would run this
    // again for any change in the package).
    println!("cargo:rerun-if-changed=Cargo.lock");

    #[cfg(target_os = "windows")]
    {
        let _ = winres::WindowsResource::new()
            .set_icon("ui/icons/logo.ico")
            .compile();
    }
}
