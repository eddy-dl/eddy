use std::process::Command;

fn main() {
    // Translations live in lang/<locale>/LC_MESSAGES/eddy-ui.po and are compiled
    // into the binary. Strings carry no per-component context, so each English
    // string is translated once wherever it appears.
    let config = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("lang")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/app.slint", config).unwrap();
    println!("cargo:rerun-if-changed=lang");

    let output = Command::new("cargo")
        .args(["pkgid", "slint"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();

    let version = output.split('@').last().unwrap_or("Unknown");

    println!("cargo:rustc-env=DEPENDENCY_SLINT_VERSION={}", version);

    #[cfg(target_os = "windows")]
    {
        let _ = winres::WindowsResource::new()
            .set_icon("ui/icons/logo.ico")
            .compile();
    }
}
