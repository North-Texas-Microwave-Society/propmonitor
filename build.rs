//! Bake the build identity into the binary.
//!
//! The self-update channel (`src/update.rs`) compares the running build's
//! commit against the one published on the release channel. The GitHub
//! Actions workflows set `PROP_BUILD_COMMIT` to the pushed commit, which
//! both identifies the build and marks it as a *dist* build — the only
//! kind eligible for unattended auto-updates. Local builds fall back to
//! `git rev-parse HEAD`, then to "dev".

use std::path::PathBuf;

fn main() {
    // Emitting any `rerun-if-*` line switches off cargo's default "rerun
    // when a file in the package changes", so without the HEAD tracking
    // below the `git_head` fallback would be resolved once and then stay
    // cached across checkouts — a local binary reporting the commit it was
    // first built at, and (if the release workflows ever gain a cargo
    // cache) a dist binary whose baked commit can never match the manifest.
    println!("cargo:rerun-if-env-changed=PROP_BUILD_COMMIT");
    track_git_head();

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
    git(&["rev-parse", "HEAD"])
}

/// One `git` invocation, trimmed stdout, `None` on anything that is not a
/// clean success (no git, no repository, no answer).
fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Re-run this script whenever `HEAD` moves, so the [`git_head`] fallback
/// tracks the checkout instead of being frozen at the first build.
///
/// Nothing is emitted outside a git working copy (a release tarball, or a
/// `cross` container that does not mount `.git`): a `rerun-if-changed` on a
/// path that does not exist makes cargo re-run the script on every build.
fn track_git_head() {
    let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]).map(PathBuf::from) else {
        return;
    };
    let head = git_dir.join("HEAD");
    if !head.is_file() {
        return;
    }
    println!("cargo:rerun-if-changed={}", head.display());

    // A detached HEAD holds the sha itself, so the file above is enough.
    // Otherwise HEAD is a symref and what actually moves on a commit,
    // checkout, pull or reset is the file it points at — unless the ref is
    // packed, in which case `packed-refs` is that file.
    let Ok(contents) = std::fs::read_to_string(&head) else {
        return;
    };
    let Some(reference) = contents.strip_prefix("ref:").map(str::trim) else {
        return;
    };
    for candidate in [git_dir.join(reference), git_dir.join("packed-refs")] {
        if candidate.is_file() {
            println!("cargo:rerun-if-changed={}", candidate.display());
            return;
        }
    }
}
