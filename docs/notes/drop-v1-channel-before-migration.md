# Dropping a V1 channel before the V2-only migration

The V2-only migration refuses to run while any V1 channel is not safely closed.
A V1 channel in `AwaitingChannelReady` can block the upgrade indefinitely, for
example when the peer never delivered its `CommitmentSigned` and the channel has
no usable local commitment transaction.

`fnn drop-v1-channel` is an offline, pre-migration tool for that case. It copies
the selected V1 channel records under the archived-V1 prefix and removes them
from the active store, so the normal migration can proceed.

## Warning

This is an **abandon**, not a close:

- it does not broadcast a commitment or closing transaction;
- it does not recover funds;
- it removes local channel state and the watchtower rows for the channel;
- if the funding transaction is already on chain, the funds can remain locked
  or be lost.

Back up the store first. Use the tool only when the funding transaction can no
longer confirm, or when you explicitly accept that the channel cannot be
recovered.

## Usage

Stop the node, then run:

```text
fnn drop-v1-channel \
  --store-path <base>/fiber/store \
  --channel-id 0x<channel-id> \
  --dry-run
```

`--dry-run` validates the selected channels and prints their state, funding
transaction/outpoint, and the number of keys that would be archived. It does not
change any records.

After reviewing the output, run the same command without `--dry-run` and confirm
the interactive prompt, or pass `--yes` for non-interactive use:

```text
fnn drop-v1-channel \
  --store-path <base>/fiber/store \
  --channel-id 0x<channel-id> \
  --yes
```

Start the node afterwards to run the normal migration. The database version is
not changed by this tool.

## What is archived

- the V1 actor record;
- the pubkey and funding-outpoint indexes;
- the opening record;
- the shutdown-settlement and pending-commit-diff records;
- the local watchtower channel and superseded-settlement rows. Client-owned
  watchtower rows (keys with an extra node id) are left untouched.

Every removed key is copied under `ARCHIVED_V1_PREFIX` before deletion.

## Availability

The subcommand is part of the `fnn` binary, so release archives and the Docker
image include it. It uses the store backend the binary was built with: RocksDB
by default, or SQLite when the binary is built with the `sqlite` feature.

The native CLI cannot open the browser store used by the WASM build. For WASM,
`fiber-wasm` exposes the same operation as:

```text
archive_v1_channels(config, database_prefix, channel_ids, apply)
```

Call it before `fiber()` so the browser database is opened without the
migration first; the subsequent `fiber()` call runs the normal migration. This
requires a custom worker: it must call `set_shared_array` before the archive
function. The bundled `fiber-js` worker calls `fiber()` directly and does not
expose this pre-migration step yet.

## Docker

The release image contains the subcommand in `/usr/local/bin/fnn`. The
entrypoint skips node setup for it, so no node password or `-d`/`-c` options are
needed:

```text
docker run --rm -it \
  -v /host/fiber:/fiber \
  nervos/fiber:<release-tag> \
  fnn drop-v1-channel \
  --store-path /fiber/fiber/store \
  --channel-id 0x... \
  --dry-run
```

The default container base directory is `/fiber`; adjust `--store-path` if your
mounted config uses a different base directory.

You can also bypass the entrypoint entirely:

```text
docker run --rm -it \
  -v /host/fiber:/fiber \
  --entrypoint fnn \
  nervos/fiber:<release-tag> \
  drop-v1-channel --store-path /fiber/fiber/store --channel-id 0x... --dry-run
```
