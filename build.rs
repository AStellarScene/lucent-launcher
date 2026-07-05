fn main() {
    glib_build_tools::compile_resources(
        &["src"],
        "src/lucent-launcher.gresource.xml",
        "lucent-launcher.gresource",
    );
}
