fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(windows)]
    if let Err(e) = winresource::WindowsResource::new()
        .set_icon("assets/icon.ico")
        .compile()
    {
        println!("cargo:warning=conflux.exe will have no icon: {e}");
    }
}
