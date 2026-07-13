#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
#[tokio::main(flavor = "current_thread")]
async fn main() {
    if platform_windows::elevated_input::run_helper()
        .await
        .is_err()
    {
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("boundless-input-injector is supported on Windows only");
}
