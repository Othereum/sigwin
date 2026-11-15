fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent".into())
        // NOT EmbedForSoftwareRenderer: it bakes only glyphs that appear in .slint files,
        // so runtime text (Korean names, messages) would lose characters
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles)
        // the Rust side (`src/i18n.rs`) reads the same catalogs and has no component names
        .with_bundled_translations("../../i18n")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("../../ui/app.slint", config).expect("slint build failed");
}
