fn main() {
    // Translations live in lang/<locale>/LC_MESSAGES/eddy-ui.po and are compiled
    // into the binary. Strings carry no per-component context, so each English
    // string is translated once wherever it appears.
    let config = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("../lang")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("app.slint", config).unwrap();
    println!("cargo:rerun-if-changed=../lang");
}
