// Embeds assets/icon.ico (the project logo) into the Windows .exe: Explorer, taskbar, Alt-Tab.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Launcher");
        res.set("FileDescription", "Launcher");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed icon: {e}");
        }
    }
}
