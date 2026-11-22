//! Windows builds get the app icon and version information embedded in the
//! executable (Explorer, taskbar, notifications, "Apps & features").

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=packaging/windows/clipstack.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let env = |key: &str| std::env::var(key).unwrap_or_default();
    let icon = std::path::Path::new(&env("CARGO_MANIFEST_DIR")).join("packaging/windows/clipstack.ico");
    let icon = icon.display().to_string().replace('\\', "/");
    let version = env("CARGO_PKG_VERSION");
    let numeric = format!(
        "{},{},{},0",
        env("CARGO_PKG_VERSION_MAJOR"),
        env("CARGO_PKG_VERSION_MINOR"),
        env("CARGO_PKG_VERSION_PATCH")
    );
    let rc = format!(
        r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Clipstack"
      VALUE "FileDescription", "Clipstack"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "Clipstack"
      VALUE "LegalCopyright", "MIT License"
      VALUE "OriginalFilename", "Clipstack.exe"
      VALUE "ProductName", "Clipstack"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let path = std::path::Path::new(&env("OUT_DIR")).join("clipstack.rc");
    std::fs::write(&path, rc).expect("write resource script");
    // Without a resource compiler (some cross builds) the exe just has no icon.
    if let Err(err) = embed_resource::compile(&path, embed_resource::NONE).manifest_optional() {
        println!("cargo::warning=Windows resources not embedded: {err}");
    }
}
