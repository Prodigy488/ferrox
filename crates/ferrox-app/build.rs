fn main() {
    // A short commit id so a build can be told apart from an older one.
    let id = std::env::var("GITHUB_SHA")
        .ok()
        .map(|s| s.chars().take(7).collect::<String>())
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "--short=7", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=FERROX_BUILD={id}");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
}
