use clap::Parser;
use fiber::store::db_migrate::DbMigrate;
use fnn_migrate::migrations::*;
use fnn_migrate::util::prompt;
use rocksdb::ops::Open;
use rocksdb::{DBCompressionType, Options, DB};
use std::path::Path;
use std::process::exit;
use std::{cmp::Ordering, sync::Arc};
use tracing::error;
use tracing_subscriber;

include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

fn init_db_migrate(db: Arc<DB>) -> DbMigrate {
    let mut db_migrate = DbMigrate::new(db);
    add_migrations(&mut db_migrate);
    db_migrate
}

fn open_db(path: &Path) -> Result<Arc<DB>, String> {
    let mut options = Options::default();
    options.create_if_missing(false);
    options.set_compression_type(DBCompressionType::Lz4);
    let db = Arc::new(DB::open(&options, path).map_err(|e| e.to_string())?);
    Ok(db)
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the database
    #[arg(short, long)]
    path: String,

    /// Skip confirmation prompts
    #[arg(short, long, default_value_t = false)]
    skip_confirm: bool,
}

fn run_migrate<P: AsRef<Path>>(
    migrate: DbMigrate,
    path: P,
    skip_confirm: bool,
) -> Result<Arc<DB>, String> {
    if let Err(_) = migrate.init_or_check(path.as_ref()) {
        let result = migrate.check();
        if result == Ordering::Less {
            if !skip_confirm {
                let path_buf = path.as_ref().to_path_buf();
                let input = prompt(format!("\
                     Once the migration started, the data will be no longer compatible with all older version,\n\
                     so we strongly recommended you to backup the old data {} before migrating.\n\
                     \n\
                     \nIf you want to migrate the data, please input YES, otherwise, the current process will exit.\n\
                     > ", path_buf.display()).as_str());

                if input.trim().to_lowercase() != "yes" {
                    error!("Migration was declined since the user didn't confirm.");
                    return Err("need to run database migration".to_string());
                }
            }
            eprintln!("begin to migrate db ...");
            let db = migrate.migrate().expect("failed to migrate db");
            eprintln!("db migrated successfully, now your can restart the fiber node ...");
            return Ok(db);
        } else {
            assert_eq!(result, Ordering::Greater);
            return Err("incompatible database, need to upgrade fiber binary".to_string());
        }
    }
    Ok(migrate.db())
}

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_target(false)
        .init();

    let args = Args::parse();
    let path = Path::new(&args.path);
    let skip_confirm = args.skip_confirm;

    let db = open_db(path).expect("failed to open db");
    let migrate = init_db_migrate(db);
    if let Err(err) = run_migrate(migrate, path, skip_confirm) {
        eprintln!("{}", err);
        exit(1);
    }
}
