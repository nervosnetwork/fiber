use std::env;
use std::fs;
use std::path::Path;

use chrono::Datelike;

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
    println!("cargo:rerun-if-changed=../../migrate/src/migrations");

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("latest_db_version.rs");

    let migrations_dir = Path::new("../../migrate/src/migrations");
    let mut migrations = Vec::new();

    let mut latest_db_version = "".to_string();
    for entry in fs::read_dir(migrations_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_file() {
            if let Some(stem) = path.file_stem() {
                if let Some(stem_str) = stem.to_str() {
                    if stem_str == "sample" || stem_str == "mod" {
                        continue;
                    }
                    migrations.push(stem_str.to_string());

                    let source_code = fs::read_to_string(&path).unwrap();
                    let version = source_code
                        .lines()
                        .find(|line| line.starts_with("const MIGRATION_DB_VERSION"))
                        .unwrap()
                        .split_whitespace()
                        .last()
                        .unwrap()
                        .replace("\";", "")
                        .replace("\"", "");
                    if version > latest_db_version {
                        latest_db_version = version;
                    }
                }
            }
        }
    }
    if latest_db_version.is_empty() {
        // If there is no migrations, `latest_db_version` is set to today.
        let now = chrono::Local::now();
        latest_db_version = format!("{:04}{:02}{:02}", now.year(), now.month(), now.day());
    }
    let mut code = String::new();
    code.push_str(&format!(
        "    pub const LATEST_DB_VERSION: &str = \"{}\";\n",
        latest_db_version
    ));
    fs::write(dest_path, code).unwrap();

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
