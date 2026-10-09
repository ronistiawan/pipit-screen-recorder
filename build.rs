// Embed the Pipit icon into the Windows .exe so Explorer, the taskbar,
// and the installer show the new branding instead of the default icon.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            eprintln!("warning: winresource failed to embed icon: {e}");
        }
    }
}
