fn main() {
    println!("cargo:rerun-if-changed=assets/app-icon.ico");

    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/app-icon.ico");
        res.compile().expect("compile tray icon resources");
    }
}
