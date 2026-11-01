fn main() {
    println!("cargo:rerun-if-changed=assets/app.manifest");
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent".into())
        // NOT EmbedForSoftwareRenderer: it bakes only glyphs that appear in .slint files,
        // so runtime text (Korean names, messages) would lose characters
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
    slint_build::compile_with_config("ui/app.slint", config).expect("slint build failed");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set_manifest_file("assets/app.manifest");
        res.set("ProductName", "Sigwin");
        res.set("FileDescription", "Sigwin (unofficial Signal client)");
        res.compile().expect("windows resource");
    }
}
