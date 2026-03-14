use crate::StoreError;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{error, info, warn};

/// Physical copy logic
///
/// # Arguments
/// * 'restore_path' - The path of Checkpoint
/// * 'target_path' - The Store path of current node
pub fn perform_physical_copy(
    restore_path: &PathBuf,
    target_path: &PathBuf,
) -> Result<(), StoreError> {
    if !restore_path.exists() {
        return Err(StoreError::DBInternalError(format!(
            "Restore source path does not exist: {:?}",
            restore_path
        )));
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| StoreError::DBInternalError(format!("System time error : {}", e)))?
        .as_secs();
    let backup_path = target_path.with_extension(format!("bak.{}", timestamp));

    info!("Starting physical database restoration.");
    info!("Source (Checkpoint): {:?}", restore_path);
    info!("Target (Current DB): {:?}", target_path);

    let mut db_was_moved = false;
    if target_path.exists() {
        info!("Moving current database to {:?}", backup_path);
        fs::rename(target_path, &backup_path)?;
        db_was_moved = true;
    }

    info!("Copying files from checkpoint to target...");
    if let Err(e) = copy_dir_all(restore_path, target_path) {
        error!(
            "Failed to copy checkpoint files: {}. Starting rollback...",
            e
        );

        if db_was_moved {
            if let Err(rollback_err) = fs::rename(&backup_path, target_path) {
                error!(
                    "CRITICAL: Rollback failed! Old database is stranded at {:?}. Error: {}",
                    backup_path, rollback_err
                );
            } else {
                warn!("Rollback successful. Original database restored.");
            }
        } else {
            let _ = fs::remove_dir_all(target_path);
        }

        return Err(e.into());
    }

    info!("Physical database swap completed successfully.");
    Ok(())
}

fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
    fs::create_dir_all(&dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_perform_physical_copy_success() {
        let source = tempdir().unwrap();
        let target = tempdir().unwrap();
        let target_db = target.path().join("db");

        fs::write(source.path().join("data.txt"), "checkpoint data").unwrap();

        fs::create_dir(&target_db).unwrap();
        fs::write(target_db.join("old.txt"), "old data").unwrap();

        let source_db = PathBuf::from(source.path());
        perform_physical_copy(&source_db, &target_db).unwrap();

        assert!(target_db.join("data.txt").exists());
        assert!(!target_db.join("old.txt").exists());

        let entries = fs::read_dir(target.path()).unwrap();
        let backup_exists = entries
            .into_iter()
            .any(|e| e.unwrap().file_name().to_string_lossy().contains("db.bak"));
        assert!(backup_exists);
    }

    #[test]
    fn test_perform_physical_copy_rollback() {
        let source_file = tempfile::NamedTempFile::new().unwrap();
        let target = tempdir().unwrap();
        let target_db = target.path().join("db");

        fs::create_dir(&target_db).unwrap();
        fs::write(target_db.join("critical.txt"), "do not lose me").unwrap();

        let source_db = PathBuf::from(source_file.path());
        let result = perform_physical_copy(&source_db, &target_db);
        assert!(result.is_err());

        assert!(target_db.join("critical.txt").exists());
    }
}
