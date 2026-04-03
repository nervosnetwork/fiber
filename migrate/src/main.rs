use clap::Parser;
use fiber_store::{db_migrate::DbMigrate, Store};
use fnn_migrate::migrations::*;
use fnn_migrate::util::prompt;
use std::cmp::Ordering;
use std::path::Path;
use std::process::exit;
use std::sync::Arc;
use tracing::error;

include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

#[ouroboros::self_referencing]
struct DbAndDbMigrate {
    db: Store,
    #[borrows(db)]
    #[covariant]
    migrate: DbMigrate<'this>,
}

fn init_db_migrate(db: Store) -> DbAndDbMigrate {
    DbAndDbMigrateBuilder {
        db,
        migrate_builder: |db| {
            let mut db_migrate = DbMigrate::new(db);
            add_migrations(&mut db_migrate);
            db_migrate
        },
    }
    .build()
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Fiber data directory (the store is at <dir>/store)
    #[arg(short = 'd', long = "dir")]
    dir: String,

    /// Skip confirmation prompts
    #[arg(short, long, default_value_t = false)]
    skip_confirm: bool,
}

fn run_migrate<P: AsRef<Path>>(
    migrate: DbAndDbMigrate,
    path: P,
    skip_confirm: bool,
) -> Result<DbAndDbMigrate, String> {
    if migrate
        .borrow_migrate()
        .init_or_check(path.as_ref())
        .is_err()
    {
        let result = migrate.borrow_migrate().check();
        if result == Ordering::Less {
            if migrate.borrow_migrate().is_any_break_change() {
                eprintln!("There is a breaking change migration, you need to shutdown all channels \
                        and restart new version fiber node with a new initialized database.\
                        You can find more information in the migration document: https://github.com/nervosnetwork/fiber/wiki/Fiber-Breaking-Change-Migration-Guide");
                return Err(
                    "need to shutdown all old channels with old version of fiber node, and then restart latest fiber node with a new database".to_string(),
                );
            }
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
            migrate
                .borrow_migrate()
                .migrate()
                .expect("failed to migrate db");
            eprintln!("db migrated successfully, now your can restart the fiber node ...");
            return Ok(migrate);
        } else {
            assert_eq!(result, Ordering::Greater);
            return Err("incompatible database, need to upgrade fiber binary".to_string());
        }
    }
    Ok(migrate)
}

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_target(false)
        .init();

    let args = Args::parse();
    let store_path = Path::new(&args.dir).join("store");
    let skip_confirm = args.skip_confirm;

    let db = Store::open_db(&store_path).expect("failed to open db");
    let migrate = init_db_migrate(db);

    if let Err(err) = run_migrate(migrate, &store_path, skip_confirm) {
        eprintln!("{}", err);
        exit(1);
    }
}
