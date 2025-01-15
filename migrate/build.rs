use std::env;
use std::fs;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=src/migrations");

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("migrations.rs");

    let migrations_dir = Path::new("src/migrations");
    let mut migrations = Vec::new();

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
                }
            }
        }
    }

    let mut code = String::new();
    code.push_str("pub fn add_migrations(db_migrate: &mut DbMigrate) {\n");

    for migration in migrations {
        code.push_str(&format!(
            "    db_migrate.add_migration(Arc::new({}::MigrationObj::new()));\n",
            migration
        ));
    }

    code.push_str("}\n");

    fs::write(dest_path, code).unwrap();
}
