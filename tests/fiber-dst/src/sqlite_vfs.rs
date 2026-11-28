//! VFS fault injection for real SQLite connections opened by Fiber DST.

use std::any::Any;
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::mem::{align_of, size_of};
use std::os::raw::{c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock, Weak};

use fiber_store::simulation::{register_vfs_resolver, BatchCommitHook, VfsBinding};
use rusqlite::ffi::{
    self, sqlite3_file, sqlite3_filename, sqlite3_int64, sqlite3_io_methods, sqlite3_vfs,
};

const VFS_NAME: &str = "fiber-dst";
type DiskRegistry = HashMap<PathBuf, Weak<Mutex<DiskState>>>;
static DISKS: LazyLock<Mutex<DiskRegistry>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static VFS: OnceLock<Result<Vfs, String>> = OnceLock::new();

struct Vfs {
    parent: usize,
    tail_offset: usize,
}

/// A one-shot filesystem error returned by SQLite's VFS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskFault {
    /// Fail a file write with SQLITE_IOERR_WRITE.
    WriteIo,
    /// Fail a file write with SQLITE_FULL.
    DiskFull,
    /// Fail a file sync with SQLITE_IOERR_FSYNC.
    SyncIo,
}

/// Select a batch by the key prefixes it puts and deletes. Supplied conditions must all match.
#[derive(Clone, Debug)]
pub struct BatchTarget {
    /// Require a put with this prefix, when supplied.
    pub put_prefix: Option<Vec<u8>>,
    /// Require a delete with this prefix, when supplied.
    pub delete_prefix: Option<Vec<u8>>,
}

#[derive(Default)]
struct DiskState {
    armed: Option<DiskFault>,
    batch: Option<BatchTarget>,
    hit: Option<DiskFault>,
    write_ops: u64,
    batch_commits: u64,
}

thread_local! {
    // SQLite calls the VFS synchronously. Other threads or connections must not
    // inherit the scope of the selected transaction.
    static ACTIVE_BATCH: Cell<*const Mutex<DiskState>> = const { Cell::new(ptr::null()) };
}

#[derive(Clone)]
struct BatchFaults(Arc<Mutex<DiskState>>);

impl std::fmt::Debug for BatchFaults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchFaults").finish_non_exhaustive()
    }
}

struct BatchScope(*const Mutex<DiskState>);

impl Drop for BatchScope {
    fn drop(&mut self) {
        ACTIVE_BATCH.set(self.0);
    }
}

impl BatchCommitHook for BatchFaults {
    fn enter_batch(
        &self,
        has_put_prefix: &dyn Fn(&[u8]) -> bool,
        has_delete_prefix: &dyn Fn(&[u8]) -> bool,
    ) -> Option<Box<dyn Any>> {
        let mut state = lock(&self.0);
        state.batch_commits = state.batch_commits.saturating_add(1);
        if state.armed.is_some()
            && state.batch.as_ref().is_some_and(|target| {
                target.put_prefix.as_deref().is_none_or(has_put_prefix)
                    && target
                        .delete_prefix
                        .as_deref()
                        .is_none_or(has_delete_prefix)
            })
        {
            Some(Box::new(BatchScope(
                ACTIVE_BATCH.replace(Arc::as_ptr(&self.0)),
            )))
        } else {
            None
        }
    }
}

/// Fault control associated with one simulated node's database directory.
#[derive(Clone)]
pub struct SimulatedDisk(Arc<DiskRegistration>);

struct DiskRegistration {
    path: PathBuf,
    state: Arc<Mutex<DiskState>>,
}

impl SimulatedDisk {
    /// Register a directory before opening its SQLite databases.
    pub fn new(path: &Path) -> Result<Self, String> {
        register_vfs_resolver(Arc::new(vfs_for));
        std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
        let path = path.canonicalize().map_err(|e| e.to_string())?;
        let state = Arc::new(Mutex::new(DiskState::default()));
        let mut disks = lock(&DISKS);
        if disks.get(&path).and_then(Weak::upgrade).is_some() {
            return Err(format!(
                "simulation disk already registered: {}",
                path.display()
            ));
        }
        disks.insert(path.clone(), Arc::downgrade(&state));
        Ok(Self(Arc::new(DiskRegistration { path, state })))
    }

    /// Arm one write or sync failure and clear the previous receipt.
    pub fn arm(&self, fault: DiskFault) {
        let mut state = lock(&self.0.state);
        state.armed = Some(fault);
        state.batch = None;
        state.hit = None;
    }

    /// Arm an error for a matching batch. It is consumed when the VFS delivers the error.
    pub fn arm_batch(&self, fault: DiskFault, target: BatchTarget) {
        let mut state = lock(&self.0.state);
        state.armed = Some(fault);
        state.batch = Some(target);
        state.hit = None;
    }

    /// Return the fault actually delivered to SQLite.
    pub fn hit(&self) -> Option<DiskFault> {
        lock(&self.0.state).hit
    }

    /// Total VFS write calls executed on this simulated node's SQLite files.
    pub fn write_ops(&self) -> u64 {
        lock(&self.0.state).write_ops
    }

    /// Total store write-batch commits executed on this simulated node.
    pub fn batch_commits(&self) -> u64 {
        lock(&self.0.state).batch_commits
    }

    /// Clear the pending fault and receipt before restarting a node.
    pub fn clear(&self) {
        let mut state = lock(&self.0.state);
        state.armed = None;
        state.batch = None;
        state.hit = None;
    }
}

impl Drop for DiskRegistration {
    fn drop(&mut self) {
        lock(&DISKS).remove(&self.path);
    }
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|error| error.into_inner())
}

fn vfs_for(path: &Path) -> Result<Option<VfsBinding>, String> {
    let disks = lock(&DISKS);
    if disks.is_empty() {
        return Ok(None);
    }
    let path = path.canonicalize().map_err(|e| e.to_string())?;
    let Some(disk) = disks.get(&path).and_then(Weak::upgrade) else {
        return Ok(None);
    };
    drop(disks);
    VFS.get_or_init(register).as_ref().map_err(Clone::clone)?;
    Ok(Some((VFS_NAME, Arc::new(BatchFaults(disk)))))
}

// The parent owns the first szOsFile bytes and receives its original file pointer
// at every call. SQLite allocates the enlarged file and keeps it alive until xClose.
// The aligned tail belongs to this shim and holds the methods and the fault state.
#[repr(C)]
struct FileTail {
    methods: sqlite3_io_methods,
    original: *const sqlite3_io_methods,
    disk: Arc<Mutex<DiskState>>,
}

fn register() -> Result<Vfs, String> {
    // SAFETY: SQLite's built-in default VFS has process lifetime. The registered
    // copy uses that VFS's data and callbacks and retains its allocation forever.
    unsafe {
        let parent = ffi::sqlite3_vfs_find(ptr::null());
        if parent.is_null() || (*parent).xOpen.is_none() {
            return Err("SQLite default VFS unavailable".to_owned());
        }
        let tail_offset = ((*parent).szOsFile as usize).next_multiple_of(align_of::<FileTail>());
        let mut vfs = Box::new(*parent);
        vfs.szOsFile =
            c_int::try_from(tail_offset + size_of::<FileTail>()).map_err(|e| e.to_string())?;
        vfs.zName = c"fiber-dst".as_ptr();
        vfs.pNext = ptr::null_mut();
        vfs.xOpen = Some(open);
        let result = ffi::sqlite3_vfs_register(&mut *vfs, 0);
        if result != ffi::SQLITE_OK {
            return Err(format!("SQLite VFS registration failed ({result})"));
        }
        Box::leak(vfs);
        Ok(Vfs {
            parent: parent as usize,
            tail_offset,
        })
    }
}

unsafe extern "C" fn open(
    _: *mut sqlite3_vfs,
    filename: sqlite3_filename,
    file: *mut sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    let Some(Ok(vfs)) = VFS.get() else {
        return ffi::SQLITE_CANTOPEN;
    };
    // SAFETY: SQLite supplies an allocation of the registered size. The parent
    // initializes its leading file and callbacks before the shim writes the tail.
    unsafe {
        let parent = vfs.parent as *mut sqlite3_vfs;
        let Some(parent_open) = (*parent).xOpen else {
            return ffi::SQLITE_CANTOPEN;
        };
        let result = parent_open(parent, filename, file, flags, out);
        if result != ffi::SQLITE_OK || filename.is_null() || (*file).pMethods.is_null() {
            return result;
        }
        let name = CStr::from_ptr(filename).to_string_lossy();
        let Some(directory) = Path::new(name.as_ref()).parent() else {
            return result;
        };
        let Some(disk) = lock(&DISKS).get(directory).and_then(Weak::upgrade) else {
            return result;
        };
        let original = (*file).pMethods;
        let mut methods = *original;
        methods.xClose = Some(close);
        methods.xWrite = Some(write);
        methods.xSync = Some(sync);
        let tail = file.cast::<u8>().add(vfs.tail_offset).cast::<FileTail>();
        tail.write(FileTail {
            methods,
            original,
            disk,
        });
        (*file).pMethods = ptr::addr_of!((*tail).methods);
        result
    }
}

fn fault(disk: &Mutex<DiskState>, sync: bool) -> Option<c_int> {
    let mut state = lock(disk);
    if !sync {
        state.write_ops = state.write_ops.saturating_add(1);
    }
    let plan = state.armed?;
    if state.batch.is_some() && !ACTIVE_BATCH.with(|active| ptr::eq(active.get(), disk)) {
        return None;
    }
    let result = match (plan, sync) {
        (DiskFault::WriteIo, false) => ffi::SQLITE_IOERR_WRITE,
        (DiskFault::DiskFull, false) => ffi::SQLITE_FULL,
        (DiskFault::SyncIo, true) => ffi::SQLITE_IOERR_FSYNC,
        _ => return None,
    };
    state.armed = None;
    state.hit = Some(plan);
    Some(result)
}

unsafe fn tail(file: *mut sqlite3_file) -> *mut FileTail {
    // SAFETY: these callbacks are installed only on a live FileTail and its
    // first repr(C) field is the methods table referenced by pMethods.
    unsafe { (*file).pMethods.cast_mut().cast::<FileTail>() }
}

unsafe extern "C" fn close(file: *mut sqlite3_file) -> c_int {
    // SAFETY: xClose is called once for an opened file. Restore the parent's
    // methods before dropping the tail and passing the still-live file back.
    unsafe {
        let tail = tail(file);
        let original = (*tail).original;
        (*file).pMethods = original;
        ptr::drop_in_place(tail);
        (*original)
            .xClose
            .map_or(ffi::SQLITE_OK, |close| close(file))
    }
}

unsafe extern "C" fn write(
    file: *mut sqlite3_file,
    data: *const c_void,
    bytes: c_int,
    offset: sqlite3_int64,
) -> c_int {
    // SAFETY: SQLite holds the file and data valid for this call. Delegate with
    // precisely the same file pointer and arguments when the operation passes.
    unsafe {
        let tail = &*tail(file);
        fault(&tail.disk, false).unwrap_or_else(|| {
            (*tail.original)
                .xWrite
                .map_or(ffi::SQLITE_IOERR_WRITE, |write| {
                    write(file, data, bytes, offset)
                })
        })
    }
}

unsafe extern "C" fn sync(file: *mut sqlite3_file, flags: c_int) -> c_int {
    // SAFETY: as for xWrite, SQLite keeps the opened file and methods alive.
    unsafe {
        let tail = &*tail(file);
        fault(&tail.disk, true).unwrap_or_else(|| {
            (*tail.original)
                .xSync
                .map_or(ffi::SQLITE_IOERR_FSYNC, |sync| sync(file, flags))
        })
    }
}

#[cfg(test)]
mod tests {
    use fiber_store::{BatchWriter, StorageBackend, Store};
    use rusqlite::{Connection, OpenFlags};

    use super::{ffi, BatchTarget, DiskFault, SimulatedDisk, VFS_NAME};

    #[test]
    fn simulated_sqlite_fault_matches_both_operations_in_one_batch() {
        let temp = tempfile::tempdir().unwrap();
        let disk = SimulatedDisk::new(temp.path()).unwrap();
        let mut store = Store::open_db(temp.path()).unwrap();
        store.set_sync_writes(true);
        store.put(b"old/key", b"original");
        store.put(b"old/sibling", b"sibling");
        disk.arm_batch(
            DiskFault::WriteIo,
            BatchTarget {
                put_prefix: Some(b"new/".to_vec()),
                delete_prefix: Some(b"old/".to_vec()),
            },
        );
        store.put(b"new/direct", b"direct");
        let mut delete_only = store.batch();
        delete_only.delete(b"old/sibling");
        delete_only.commit();
        let mut put_only = store.batch();
        put_only.put(b"new/sibling", b"sibling");
        put_only.commit();
        assert_eq!(disk.hit(), None);
        let mut moving = store.batch();
        moving.delete(b"old/key");
        moving.put(b"new/key", b"moved");
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| moving.commit())).is_err()
        );
        assert_eq!(disk.hit(), Some(DiskFault::WriteIo));
        drop(store);
        let store = Store::open_db(temp.path()).unwrap();
        assert_eq!(
            store.get(b"old/key").as_deref(),
            Some(b"original".as_slice())
        );
        assert_eq!(store.get(b"new/key"), None);
        assert_eq!(store.get(b"old/sibling"), None);
        assert_eq!(
            store.get(b"new/direct").as_deref(),
            Some(b"direct".as_slice())
        );
        assert_eq!(
            store.get(b"new/sibling").as_deref(),
            Some(b"sibling".as_slice())
        );
    }

    #[test]
    fn simulated_sqlite_faults_are_node_local_and_batches_are_atomic() {
        for (fault, expected) in [
            (DiskFault::WriteIo, ffi::SQLITE_IOERR_WRITE),
            (DiskFault::DiskFull, ffi::SQLITE_FULL),
            (DiskFault::SyncIo, ffi::SQLITE_IOERR_FSYNC),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let a_dir = temp.path().join("a");
            let b_dir = temp.path().join("b");
            let a_disk = SimulatedDisk::new(&a_dir).unwrap();
            let b_disk = SimulatedDisk::new(&b_dir).unwrap();
            assert_eq!(super::vfs_for(&a_dir).unwrap().unwrap().0, VFS_NAME);
            let open = |path: &std::path::Path| {
                Connection::open_with_flags_and_vfs(
                    path.join("data.sqlite"),
                    OpenFlags::default(),
                    VFS_NAME,
                )
                .unwrap()
            };
            let mut a = open(&a_dir);
            let b = open(&b_dir);
            for node in [&a, &b] {
                node.execute_batch(
                    "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
                     CREATE TABLE first_value (value INTEGER);
                     CREATE TABLE second_value (value INTEGER);
                     INSERT INTO first_value VALUES (0);
                     INSERT INTO second_value VALUES (0);",
                )
                .unwrap();
            }
            a_disk.arm(fault);
            b.execute("UPDATE first_value SET value=1", []).unwrap();
            assert_eq!(a_disk.hit(), None);
            assert_eq!(b_disk.hit(), None);
            let transaction = a.transaction().unwrap();
            transaction
                .execute("UPDATE first_value SET value=1", [])
                .unwrap();
            transaction
                .execute("UPDATE second_value SET value=1", [])
                .unwrap();
            let error = transaction.commit().unwrap_err();
            assert_eq!(error.sqlite_error().unwrap().extended_code, expected);
            assert_eq!(a_disk.hit(), Some(fault));
            drop(a);
            a_disk.clear();
            let a = open(&a_dir);
            let values: (i64, i64) = a
                .query_row(
                    "SELECT first_value.value, second_value.value FROM first_value, second_value",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert!(
                matches!(values, (0, 0) | (1, 1)),
                "partial commit: {values:?}"
            );
        }
    }
}
