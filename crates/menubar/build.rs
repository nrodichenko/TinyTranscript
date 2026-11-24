// The build version shown in the menu and the app's Info.plist: TT_VERSION (YYYYMMDD.N, set by
// install.sh / package.sh), or "dev" for a plain cargo build.
fn main() {
    println!("cargo:rerun-if-env-changed=TT_VERSION");
    let v = std::env::var("TT_VERSION").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "dev".into());
    println!("cargo:rustc-env=TT_BUILD_VERSION={v}");
    // owner/name of the GitHub repository whose releases are the updates ("" = no updates)
    println!("cargo:rerun-if-env-changed=TT_UPDATE_REPO");
    println!("cargo:rustc-env=TT_UPDATE_REPO={}", std::env::var("TT_UPDATE_REPO").unwrap_or_default());
}
