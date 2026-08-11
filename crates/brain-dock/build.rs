fn main() {
    let ui = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../ui")
        .join("dock.slint");

    slint_build::compile(&ui).expect("compiling ui/dock.slint");
}
