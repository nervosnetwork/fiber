# Archived Migrations (v0.7.x and earlier)

This directory contains the **archived** standalone migration tool (`fnn-migrate`)
that was used for database migrations up to v0.7.x.

**This code is NOT compiled by CI and is kept for reference only.**

## Why archived?

The migration system has been unified into `crates/fiber-store/`. All new
migrations are written against the `StorageBackend` trait and work across
all backends (RocksDB, SQLite, IndexedDB).

## Upgrading from old databases

If you have a database older than version `20260302100001`, you must first
upgrade using the v0.7.x `fnn-migrate` binary:

1. Download the v0.7.x release binary from GitHub releases
2. Run: `fnn-migrate -d <data-dir>`
3. Then upgrade to the new fiber version

The new migration system will take over from version `20260302100001` onwards.
