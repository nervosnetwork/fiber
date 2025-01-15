use std::env;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=migration/src/migrations");

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("latest_db_version.rs");

    let migrations_dir = Path::new("migration/src/migrations");
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

    let mut code = String::new();
    code.push_str(&format!(
        "    pub const LATEST_DB_VERSION: &str = \"{}\";\n",
        latest_db_version
    ));
    eprintln!("latest_db_version: {}", latest_db_version);
    eprintln!("dest_path: {:?}", dest_path);
    fs::write(dest_path, code).unwrap();
}
