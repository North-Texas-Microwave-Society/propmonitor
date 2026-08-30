//! Bake the build identity into the binary.
//!
//! The self-update channel (`src/update.rs`) compares the running build's
//! commit against the one published on the release channel. The GitHub
//! Actions workflows set `PROP_BUILD_COMMIT` to the pushed commit, which
//! both identifies the build and marks it as a *dist* build — the only
//! kind eligible for unattended auto-updates. Local builds fall back to
//! `git rev-parse HEAD`, then to "dev".

fn main() {
    println!("cargo:rerun-if-env-changed=PROP_BUILD_COMMIT");

    let commit = std::env::var("PROP_BUILD_COMMIT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(git_head)
        .unwrap_or_else(|| "dev".to_string());

    let dist = std::env::var("PROP_BUILD_COMMIT").is_ok_and(|c| !c.is_empty());

    println!("cargo:rustc-env=PROP_BUILD_COMMIT={commit}");
    println!(
        "cargo:rustc-env=PROP_DIST_BUILD={}",
        if dist { "1" } else { "0" }
    );
}

/// The commit the source tree was checked out at, when building from a
/// git working copy.
fn git_head() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!sha.is_empty()).then_some(sha)
}
