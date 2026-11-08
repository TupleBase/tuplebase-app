fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Windows reads the app icon from the executable. GPUI loads resource ID 1 for its
    // windows, and Explorer uses the lowest ID.
    #[cfg(windows)]
    {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let icon = format!("{manifest_dir}/../../assets/icon/TupleBase.ico");
        println!("cargo:rerun-if-changed={icon}");
        winresource::WindowsResource::new()
            .set_icon_with_id(&icon, "1")
            .compile()
            .expect("failed to embed the Windows icon");
    }
}
