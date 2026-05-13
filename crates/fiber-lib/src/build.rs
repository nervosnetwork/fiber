use std::path::Path;

fn rerun_if_changed(path_str: &str) -> bool {
    let path = Path::new(path_str);

    if path.starts_with("benches")
        || path.starts_with("migrate")
        || path.starts_with("docker")
        || path.starts_with("docs")
        || path.starts_with("test")
        || path.starts_with(".github")
        || path.ends_with("tests.rs")
    {
        return false;
    }

    for ancestor in path.ancestors() {
        if ancestor.ends_with("tests") {
            return false;
        }
    }

    !matches!(
        path_str,
        "COPYING" | "Makefile" | "clippy.toml" | "rustfmt.toml" | "rust-toolchain"
    )
}

/// Gets the field [`commit_describe`] via Git.
///
/// [`commit_describe`]: struct.Version.html#structfield.commit_describe
pub fn get_commit_describe() -> Option<String> {
    std::process::Command::new("git")
        .args([
            "describe",
            "--dirty",
            "--always",
            "--match",
            "__EXCLUDE__",
            "--abbrev=7",
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|r| {
            String::from_utf8(r.stdout)
                .ok()
                .map(|s| s.trim().to_string())
        })
}

/// Gets the field [`commit_date`] via Git.
///
/// [`commit_date`]: struct.Version.html#structfield.commit_date
pub fn get_commit_date() -> Option<String> {
    std::process::Command::new("git")
        .env("TZ", "UTC")
        .args(["log", "-1", "--date=iso", "--pretty=format:%cd"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|r| {
            String::from_utf8(r.stdout)
                .ok()
                .map(|s| s.trim()[..10].to_string())
        })
}

#[allow(clippy::manual_strip)]
fn main() {
    let files_stdout = std::process::Command::new("git")
        .args(["ls-tree", "-r", "--name-only", "HEAD"])
        .output()
        .ok()
        .and_then(|r| String::from_utf8(r.stdout).ok());

    if files_stdout.is_some() {
        println!(
            "cargo:rustc-env=GIT_COMMIT_HASH={}",
            get_commit_describe().unwrap_or_default()
        );
        println!(
            "cargo:rustc-env=GIT_COMMIT_DATE={}",
            get_commit_date().unwrap_or_default()
        );

        let git_head = std::process::Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .output()
            .ok()
            .and_then(|r| String::from_utf8(r.stdout).ok())
            .and_then(|s| s.lines().next().map(ToOwned::to_owned))
            .map(|ref s| Path::new(s).to_path_buf())
            .unwrap_or_else(|| Path::new(".git").to_path_buf())
            .join("HEAD");
        if git_head.exists() {
            println!("cargo:rerun-if-changed={}", git_head.display());

            let head = std::fs::read_to_string(&git_head).unwrap_or_default();
            if head.starts_with("ref: ") {
                let path_str = format!(".git/{}", head[5..].trim());
                let path = Path::new(&path_str);
                if path.exists() {
                    println!("cargo:rerun-if-changed={path_str}");
                }
            }
        }
    }

    for file in files_stdout.iter().flat_map(|stdout| stdout.lines()) {
        if rerun_if_changed(file) {
            println!("cargo:rerun-if-changed={file}");
        }
    }
}
