use std::env;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=src/migrations");

    let out_dir = env::var("OUT_DIR").unwrap();
    let migrations_dir = Path::new("src/migrations");

    let mut latest_db_version = String::new();
    let mut migration_modules = Vec::new();

    if migrations_dir.exists() {
        for entry in fs::read_dir(migrations_dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_file() {
                if let Some(stem) = path.file_stem() {
                    if let Some(stem_str) = stem.to_str() {
                        if stem_str == "mod" {
                            continue;
                        }

                        let source_code = fs::read_to_string(&path).unwrap();
                        if let Some(version_line) = source_code
                            .lines()
                            .find(|line| line.starts_with("const MIGRATION_DB_VERSION"))
                        {
                            let version = version_line
                                .split_whitespace()
                                .last()
                                .unwrap()
                                .replace("\";", "")
                                .replace('"', "");
                            if version > latest_db_version {
                                latest_db_version = version;
                            }
                            migration_modules.push(stem_str.to_string());
                        }
                    }
                }
            }
        }
    }

    // Sort modules by name (which is by version since they follow mig_YYYYMMDD_ naming)
    migration_modules.sort();

    // Generate LATEST_DB_VERSION constant
    // If no migrations exist, use INIT_DB_VERSION as the latest
    let version_code = if latest_db_version.is_empty() {
        "    pub const LATEST_DB_VERSION: &str = INIT_DB_VERSION;\n".to_string()
    } else {
        format!(
            "    pub const LATEST_DB_VERSION: &str = \"{}\";\n",
            latest_db_version
        )
    };
    let version_path = Path::new(&out_dir).join("latest_db_version.rs");
    fs::write(version_path, version_code).unwrap();

    // Generate register_migrations function
    let mut reg_code = String::new();
    let manifest_dir = env::var("CARGO_MANIFEST_DIR")
        .unwrap()
        .escape_default()
        .to_string();

    // Module declarations with #[path] to locate files in src/migrations/
    // Using #[path] because mod declarations inside include!-ed files
    // resolve relative to the included file's directory (OUT_DIR), not the caller.
    for module in &migration_modules {
        reg_code.push_str(&format!(
            "#[path = \"{}/src/migrations/{}.rs\"]\nmod {};\n",
            manifest_dir, module, module
        ));
    }
    reg_code.push('\n');

    // Registration function
    let param_name = if migration_modules.is_empty() {
        "_db_migrate"
    } else {
        "db_migrate"
    };
    reg_code.push_str(&format!(
        "pub fn register_all_migrations({}: &mut DbMigrate) {{\n",
        param_name
    ));
    for module in &migration_modules {
        reg_code.push_str(&format!(
            "    db_migrate.add_migration(Arc::new({}::MigrationObj::new()));\n",
            module
        ));
    }
    reg_code.push_str("}\n");

    let reg_path = Path::new(&out_dir).join("register_migrations.rs");
    fs::write(reg_path, reg_code).unwrap();
}
