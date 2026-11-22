//! Embeds the icon and version information into the Windows executable.

fn main() {
    println!("cargo:rerun-if-changed=assets/icons/magic-pdf.ico");
    // Only for MSVC builds (official Windows builds); needs the Windows SDK's rc.exe.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icons/magic-pdf.ico")
            .set("ProductName", "magic-pdf")
            .set("FileDescription", "magic-pdf PDF viewer")
            .set("CompanyName", "magic-pdf contributors")
            .set("LegalCopyright", "AGPL-3.0-or-later");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the Windows icon: {e}");
        }
    }
}
