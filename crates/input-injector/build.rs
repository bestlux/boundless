fn main() {
    println!("cargo:rerun-if-changed=assets/input-injector.manifest");
    #[cfg(windows)]
    {
        let mut resource = winres::WindowsResource::new();
        let version = version_resource_value(
            env!("CARGO_PKG_VERSION_MAJOR"),
            env!("CARGO_PKG_VERSION_MINOR"),
            env!("CARGO_PKG_VERSION_PATCH"),
        );
        resource
            .set_manifest_file("assets/input-injector.manifest")
            .set("FileDescription", "Boundless elevated input injector")
            .set("ProductName", "Boundless")
            .set("InternalName", "boundless-input-injector.exe")
            .set("OriginalFilename", "boundless-input-injector.exe")
            .set("FileVersion", env!("CARGO_PKG_VERSION"))
            .set("ProductVersion", env!("CARGO_PKG_VERSION"))
            .set_version_info(winres::VersionInfo::FILEVERSION, version)
            .set_version_info(winres::VersionInfo::PRODUCTVERSION, version);
        resource
            .compile()
            .expect("compile elevated input injector manifest");
    }
}

#[cfg(windows)]
fn version_resource_value(major: &str, minor: &str, patch: &str) -> u64 {
    let component = |value: &str| value.parse::<u16>().unwrap_or(0) as u64;
    (component(major) << 48) | (component(minor) << 32) | (component(patch) << 16)
}
