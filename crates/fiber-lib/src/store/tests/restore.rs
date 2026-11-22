use std::path::{Path, PathBuf};

use fiber_store::StorageBackend;
use fiber_types::Hash256;

use crate::gen_rand_sha256_hash;
use crate::invoice::PreimageStore;
use crate::store::restore::restore;
use crate::store::{open_store, Store};
use crate::tests::test_utils::TempDir;

fn random_preimage() -> (Hash256, Hash256) {
    (gen_rand_sha256_hash(), gen_rand_sha256_hash())
}

/// Writes `preimages` to a new store in `dir`, backs it up to `dir/backup`, and returns the
/// backup's path.
fn backup_with(dir: &Path, preimages: &[(Hash256, Hash256)]) -> PathBuf {
    let source = open_store(dir.join("source")).expect("open source store");
    for (payment_hash, preimage) in preimages {
        source.insert_preimage(*payment_hash, *preimage);
    }
    let backup = dir.join("backup");
    source.backup(&backup).expect("back up source store");
    backup
}

/// Restores `backup` into `store_path`. The backup holds no key files, so `restore()` leaves
/// the key paths alone.
fn restore_store(backup: &Path, store_path: &Path) -> crate::errors::Result<()> {
    let dir = store_path.parent().expect("store folder has a parent");
    restore(backup, store_path, &dir.join("sk"), &dir.join("key"))
}

/// The entries named `*.bak.*` beside or inside the store folder: what a restore moved aside.
fn moved_aside(store_path: &Path) -> Vec<PathBuf> {
    let parent = store_path.parent().expect("store folder has a parent");
    let mut paths = Vec::new();
    for dir in [parent, store_path] {
        for entry in std::fs::read_dir(dir).expect("list folder") {
            let path = entry.expect("read folder entry").path();
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().contains(".bak."))
            {
                paths.push(path);
            }
        }
    }
    paths
}

fn is_empty_dir(path: &Path) -> bool {
    path.is_dir()
        && std::fs::read_dir(path)
            .expect("list folder")
            .next()
            .is_none()
}

/// Opens the store that a restore moved aside. RocksDB moves the whole store folder.
#[cfg(not(feature = "sqlite"))]
fn open_moved_aside(moved: &Path, _scratch: &Path) -> Store {
    open_store(moved).expect("open the moved-aside store")
}

/// Opens the store that a restore moved aside. SQLite moves only the database file, so it is
/// copied into a folder of its own first.
#[cfg(feature = "sqlite")]
fn open_moved_aside(moved: &Path, scratch: &Path) -> Store {
    let folder = scratch.join("moved_aside");
    std::fs::create_dir(&folder).expect("create folder");
    std::fs::copy(moved, folder.join("data.sqlite")).expect("copy the moved-aside database");
    open_store(&folder).expect("open the moved-aside store")
}

/// A fresh node's store folder exists and is empty. The restored store opens with the backup's
/// data, and no store was created in the folder before the copy.
#[test]
fn test_restore_into_fresh_store_dir() {
    let dir = TempDir::new("test-restore-fresh-store-dir");
    let root: &Path = dir.as_ref();
    let (payment_hash, preimage) = random_preimage();
    let backup = backup_with(root, &[(payment_hash, preimage)]);
    let store_path = root.join("store");
    std::fs::create_dir(&store_path).expect("create store folder");

    restore_store(&backup, &store_path).expect("restore");

    let store = open_store(&store_path).expect("open restored store");
    assert_eq!(store.get_preimage(&payment_hash), Some(preimage));
    // RocksDB moves the empty folder aside, and SQLite finds no database file to move.
    for path in moved_aside(&store_path) {
        assert!(
            is_empty_dir(&path),
            "a store was created before the copy: {path:?}"
        );
    }
}

/// The store folder doesn't exist yet. The restore creates it with the backup's data, and
/// there is nothing to move aside.
#[test]
fn test_restore_into_missing_store_dir() {
    let dir = TempDir::new("test-restore-missing-store-dir");
    let root: &Path = dir.as_ref();
    let (payment_hash, preimage) = random_preimage();
    let backup = backup_with(root, &[(payment_hash, preimage)]);
    let store_path = root.join("store");

    restore_store(&backup, &store_path).expect("restore");

    assert!(store_path.is_dir());
    let store = open_store(&store_path).expect("open restored store");
    assert_eq!(store.get_preimage(&payment_hash), Some(preimage));
    let moved = moved_aside(&store_path);
    assert!(
        moved.is_empty(),
        "a store was created before the copy: {moved:?}"
    );
}

/// Restoring over an existing node: its store is moved aside with its data, and the restored
/// store holds only the backup's data.
#[test]
fn test_restore_over_existing_store_moves_it_aside() {
    let dir = TempDir::new("test-restore-over-existing-store");
    let root: &Path = dir.as_ref();
    let (first_hash, first_preimage) = random_preimage();
    let (second_hash, second_preimage) = random_preimage();
    let (old_hash, old_preimage) = random_preimage();

    // Two backups, so that with RocksDB the backup holds two table files.
    let source = open_store(root.join("source")).expect("open source store");
    source.insert_preimage(first_hash, first_preimage);
    source
        .backup(&root.join("first_backup"))
        .expect("first backup");
    source.insert_preimage(second_hash, second_preimage);
    let backup = root.join("backup");
    source.backup(&backup).expect("back up source store");
    drop(source);

    let store_path = root.join("store");
    let existing = open_store(&store_path).expect("open existing store");
    existing.insert_preimage(old_hash, old_preimage);
    drop(existing);

    restore_store(&backup, &store_path).expect("restore");

    let store = open_store(&store_path).expect("open restored store");
    assert_eq!(store.get_preimage(&first_hash), Some(first_preimage));
    assert_eq!(store.get_preimage(&second_hash), Some(second_preimage));
    assert_eq!(store.get_preimage(&old_hash), None);
    let moved = moved_aside(&store_path);
    assert_eq!(moved.len(), 1, "expected one store moved aside: {moved:?}");
    let old = open_moved_aside(&moved[0], root);
    assert_eq!(old.get_preimage(&old_hash), Some(old_preimage));
}
