use std::path::Path;
use std::process::Command;

fn main() {
    let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| {
        println!(
            "cargo:warning=could not read the git commit hash, so --version reports it as \
             `unknown`; build from a git checkout with git on PATH to embed it"
        );
        "unknown".to_owned()
    });
    println!("cargo:rustc-env=GIT_COMMIT={commit}");
    rerun_when_head_moves();
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    Some(stdout.trim().to_owned())
}

/// Cargo reruns the script for a missing watched path on every build, so only
/// existing files are watched.
fn rerun_when_head_moves() {
    let mut refs = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
    refs.extend(git(&["symbolic-ref", "-q", "HEAD"]));
    for name in refs {
        let Some(path) = git(&["rev-parse", "--git-path", &name]) else {
            continue;
        };
        if Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
