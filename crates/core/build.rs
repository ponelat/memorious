//! Captures build-time metadata (git commit, build date) as compile-time env
//! vars, consumed by `crate::GIT_SHA`/`crate::BUILD_DATE`/`crate::BUILD_INFO`
//! in `src/lib.rs` and shown in every client's "about"/status screen
//! (`Node::status_json`'s `git_sha`/`build_date`, next to the existing
//! `version`). Best-effort only, on purpose: a packaged build with no
//! `.git` — a Nix flake's sandboxed `src = self` strips it, same reason
//! `VERSION` in lib.rs uses the Cargo version rather than a git hash (see
//! LOG.md 2026-09-17) — falls back to env var overrides the packaging sets,
//! then to "unknown". This must never fail the build.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=MEMORIOUS_GIT_SHA");
    println!("cargo:rerun-if-env-changed=MEMORIOUS_BUILD_DATE");

    println!("cargo:rustc-env=MEMORIOUS_GIT_SHA={}", git_sha());
    println!("cargo:rustc-env=MEMORIOUS_BUILD_DATE={}", build_date());
}

/// The packaging env var wins outright (flake.nix passes `self.shortRev`
/// this way, since the sandbox has no `.git` to ask); otherwise ask git
/// directly, appending `-dirty` for an uncommitted worktree so a dev build
/// never looks like a clean release. "unknown" if neither is available.
fn git_sha() -> String {
    if let Some(sha) = non_empty_env("MEMORIOUS_GIT_SHA") {
        return sha;
    }
    let Some(dir) = run(&["rev-parse", "--absolute-git-dir"]) else {
        return "unknown".into();
    };
    // Rerun when the commit (or the index, for the dirty check below)
    // changes — a submodule's ".git" is a file pointing elsewhere, so this
    // must come from `--absolute-git-dir`, not a guessed relative path.
    println!("cargo:rerun-if-changed={dir}/HEAD");
    println!("cargo:rerun-if-changed={dir}/index");
    let Some(mut sha) = run(&["rev-parse", "--short=10", "HEAD"]) else {
        return "unknown".into();
    };
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    if dirty {
        sha.push_str("-dirty");
    }
    sha
}

/// UTC `YYYY-MM-DD`. The packaging env var wins (a Nix build should be
/// reproducible, not stamped with whenever the sandbox happened to run);
/// otherwise the host's `date`. "unknown" if neither works.
fn build_date() -> String {
    non_empty_env("MEMORIOUS_BUILD_DATE").unwrap_or_else(|| {
        Command::new("date")
            .args(["-u", "+%Y-%m-%d"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".into())
    })
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn run(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}
