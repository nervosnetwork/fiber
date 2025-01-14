use clap::Parser;
use fiber::store::db_migrate::DbMigrate;
use fnn_migrate::migrations::*;
use rocksdb::ops::Open;
use rocksdb::{DBCompressionType, Options, DB};
use std::path::Path;
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
}

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_target(false)
        .init();

    let args = Args::parse();
    let path = Path::new(&args.path);

    let db = open_db(path).expect("failed to open db");
    let migrate = init_db_migrate(db);
    if let Err(err) = migrate.check_or_run_migrate(path, true) {
        eprintln!("{}", err);
    }
}
