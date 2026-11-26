#![cfg(not(target_arch = "wasm32"))]

use std::io::Write;
use std::process::{Command, Stdio};

use fiber_store::migration::MIGRATION_VERSION_KEY;
use fiber_store::{StorageBackend, Store};
use fiber_types_0100::sample::StoreSample;
use fiber_types_0100::{AwaitingChannelReadyFlags, ChannelActorData, ChannelState};

const RELEASED_VERSION: &str = "20260925120000";

fn bytes(id: &str) -> Vec<u8> {
    let hex = id.strip_prefix("0x").unwrap_or(id);
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::from("0x");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn create_v1_store() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_db(dir.path()).unwrap();
    store.put(MIGRATION_VERSION_KEY, RELEASED_VERSION.as_bytes());

    let mut channel = ChannelActorData::samples(1).remove(0);
    channel.state = ChannelState::AwaitingChannelReady(AwaitingChannelReadyFlags::empty());
    let id = channel.id;
    store.put(
        [&[0u8], id.as_ref()].concat(),
        bincode::serialize(&channel).unwrap(),
    );
    (dir, hex(id.as_ref()))
}

fn run_tool(args: &[&str], stdin: Stdio) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_fnn"))
        .arg("drop-v1-channel")
        .args(args)
        .stdin(stdin)
        .output()
        .expect("run fnn drop-v1-channel")
}

#[test]
fn rejects_a_path_that_is_not_an_existing_store() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("not-a-store");
    let output = run_tool(
        &[
            "--store-path",
            missing.to_str().unwrap(),
            "--channel-id",
            &format!("0x{}", "11".repeat(32)),
            "--yes",
        ],
        Stdio::null(),
    );
    assert!(!output.status.success());
    assert!(!missing.exists(), "the tool must not create a database");

    let existing = dir.path().join("existing-empty");
    std::fs::create_dir(&existing).unwrap();
    let output = run_tool(
        &[
            "--store-path",
            existing.to_str().unwrap(),
            "--channel-id",
            &format!("0x{}", "11".repeat(32)),
            "--yes",
        ],
        Stdio::null(),
    );
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read_dir(&existing).unwrap().count(),
        0,
        "the tool must not create files in an existing non-store directory"
    );
}

#[test]
fn dry_run_does_not_prompt_or_write() {
    let (dir, id) = create_v1_store();
    let output = run_tool(
        &[
            "--store-path",
            dir.path().to_str().unwrap(),
            "--channel-id",
            &id,
            "--dry-run",
        ],
        Stdio::null(),
    );
    assert!(output.status.success(), "{output:?}");
    let store = Store::open_db(dir.path()).unwrap();
    let actor_key = [&[0u8][..], bytes(&id).as_slice()].concat();
    assert!(store.get(&actor_key).is_some());
}

#[test]
fn confirmation_gate_and_apply_archive_the_channel() {
    let (dir, id) = create_v1_store();
    let mut child = Command::new(env!("CARGO_BIN_EXE_fnn"))
        .arg("drop-v1-channel")
        .args([
            "--store-path",
            dir.path().to_str().unwrap(),
            "--channel-id",
            &id,
        ])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn tool");
    child.stdin.as_mut().unwrap().write_all(b"no\n").unwrap();
    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "a rejected confirmation must not archive"
    );
    let store = Store::open_db(dir.path()).unwrap();
    let actor_key = [&[0u8][..], bytes(&id).as_slice()].concat();
    assert!(store.get(&actor_key).is_some());
    drop(store);

    let output = run_tool(
        &[
            "--store-path",
            dir.path().to_str().unwrap(),
            "--channel-id",
            &id,
            "--yes",
        ],
        Stdio::null(),
    );
    assert!(output.status.success(), "{output:?}");
    let store = Store::open_db(dir.path()).unwrap();
    assert!(store.get(&actor_key).is_none());
    let archived = [&[240u8][..], actor_key.as_slice()].concat();
    assert!(store.get(&archived).is_some());
}
