use std::{env, fs, path::PathBuf, process::Command};
fn main() {
    println!("cargo:rerun-if-changed=assets/filebackup.ico");
    println!("cargo:rerun-if-changed=assets/icon.rc");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let sdk = PathBuf::from(
        env::var_os("ProgramFiles(x86)").expect("Windows SDK requires ProgramFiles(x86)"),
    )
    .join("Windows Kits/10/bin");
    let compiler = fs::read_dir(sdk)
        .expect("Install the Windows SDK")
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("x64/rc.exe"))
        .filter(|p| p.is_file())
        .max()
        .expect("Windows SDK resource compiler not found");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("filebackup.res");
    let status = Command::new(compiler)
        .args(["/nologo", "/fo"])
        .arg(&output)
        .arg("assets/icon.rc")
        .status()
        .expect("Could not run Windows resource compiler");
    assert!(status.success(), "Icon resource compilation failed");
    println!("cargo:rustc-link-arg={}", output.display());
}
