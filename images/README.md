# Archived release images

This directory holds one **seeded** disk image per shipped TablesOS version. They
exist so the "top up version" upgrade path (Table List → `[u]`, see
[`kernel/src/upgrade.rs`](../kernel/src/upgrade.rs) and
[`tablestore/src/migrate.rs`](../tablestore/src/migrate.rs)) can be tested by
flashing an older image and upgrading it to the version you just built.

| File | Version | Notes |
|------|---------|-------|
| `tablesos-v0.1.0-seed.img` | v0.1.0 | First release; seeded test dataset. |

## Retention policy

**Whenever the workspace version is bumped, archive the outgoing release first,
then bump.** That keeps every previously shipped version available to upgrade
*from*. To produce an archive:

```sh
# builds target/tablesos.img and copies it to images/tablesos-<version>-seed.img
cargo run -- --seed --no-run --archive
```

These are 64 MiB binaries. Keep them out of regular commits unless you
deliberately want them versioned; they are build artifacts, not source.

## How to test an upgrade

1. Flash an older image onto the emulated USB stick (or a real pendrive):
   `cp images/tablesos-v0.1.0-seed.img target/usbstick.img`
2. Boot the current version (`cargo run` or `cargo run -- --uefi`).
3. From the Table List press `[u]`, pick the USB volume, confirm.
4. The stick is upgraded in place to the current version with its seeded
   tables migrated and preserved.
