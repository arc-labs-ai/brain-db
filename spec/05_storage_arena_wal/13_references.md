# 05.13 References

References for the storage layer.

## 1. Linux kernel I/O primitives

### io_uring

- **`liburing`** — userspace library for io_uring. [GitHub: axboe/liburing](https://github.com/axboe/liburing). Maintained by Jens Axboe.

- **`io_uring_prep_writev2`** — vectored-write submission helper. [Source on GitHub](https://github.com/axboe/liburing/blob/master/man/io_uring_prep_writev2.3).

### Kernel UAPI headers

- **`include/uapi/linux/fs.h`** — definitions for `RWF_DSYNC`, `FICLONE`, `FICLONERANGE`. [Source on GitHub](https://github.com/torvalds/linux/blob/master/include/uapi/linux/fs.h).

- **`include/uapi/asm-generic/fcntl.h`** — definitions for `O_DIRECT`. [Source on GitHub](https://github.com/torvalds/linux/blob/master/include/uapi/asm-generic/fcntl.h).

- **`include/uapi/linux/falloc.h`** — definitions for `fallocate` flags. [Source on GitHub](https://github.com/torvalds/linux/blob/master/include/uapi/linux/falloc.h).

### Linux kernel documentation

- **Documentation/admin-guide/mm/transhuge.rst** — Transparent Huge Pages. The source for our claim that THP doesn't apply to file-backed mmaps on regular filesystems. [Source on GitHub](https://github.com/torvalds/linux/blob/master/Documentation/admin-guide/mm/transhuge.rst).

- **Documentation/filesystems/btrfs.rst** — btrfs documentation. [Source on GitHub](https://github.com/torvalds/linux/blob/master/Documentation/filesystems/btrfs.rst).

### Reference

- **Michael Kerrisk, "The Linux Programming Interface" (No Starch Press, 2010).** [tlpi book site](http://man7.org/tlpi/). The standard reference for Linux system programming.

## 2. The metadata store

- **redb** — pure-Rust embedded ACID key-value store. [GitHub: cberner/redb](https://github.com/cberner/redb).

  - Documentation in the repo's README and the [crate docs](https://docs.rs/redb).

## 3. Async runtime

- **glommio** — DataDog's thread-per-core async runtime. [GitHub: DataDog/glommio](https://github.com/DataDog/glommio). Linux-only; requires kernel ≥ 5.8.

- **Seastar** — the C++ thread-per-core framework underlying ScyllaDB. [GitHub: scylladb/seastar](https://github.com/scylladb/seastar). The original lineage Glommio draws from.

- **ScyllaDB** — production-scale demonstration of thread-per-core architecture. [GitHub: scylladb/scylladb](https://github.com/scylladb/scylladb).

## 4. Hashing

- **CRC32C** — used for header and payload checksums. The polynomial is 0x1EDC6F41. SSE 4.2 (`_mm_crc32_*` intrinsics) and ARMv8.0+ have hardware acceleration.

  - **`crc32fast`** — Rust implementation with hardware acceleration. [GitHub: srijs/rust-crc32fast](https://github.com/srijs/rust-crc32fast).

- **BLAKE3** — used for fingerprinting and snapshot integrity. [GitHub: BLAKE3-team/BLAKE3](https://github.com/BLAKE3-team/BLAKE3).

## 5. WAL design background

- **PostgreSQL's WAL** — a long-running real-world example of write-ahead logging. The PostgreSQL WAL is more sophisticated than Brain's (handles much more complex transactional semantics) but the principles are similar. [postgresql.org/docs/current/wal.html](https://www.postgresql.org/docs/current/wal.html).

- **MySQL's InnoDB redo log** — another mature WAL implementation. [dev.mysql.com/doc/refman/8.0/en/innodb-redo-log.html](https://dev.mysql.com/doc/refman/8.0/en/innodb-redo-log.html).

- **Aries** — Mohan et al., 1992. ["ARIES: A Transaction Recovery Method Supporting Fine-Granularity Locking and Partial Rollbacks Using Write-Ahead Logging"](https://dl.acm.org/doi/10.1145/128765.128770). The classic recovery algorithm; Brain's recovery is simpler than ARIES but inspired by its principles.

## 6. mmap discussion

- **Andy Pavlo et al., "Are You Sure You Want to Use MMAP in Your Database Management System?" (CIDR 2022)** — a critical look at mmap for databases. [Source PDF](https://db.cs.cmu.edu/mmap-cidr2022/). Worth reading before defending Brain's mmap choice; the criticisms are real but workable for our access patterns.

## 7. The reflink mechanism

- **btrfs documentation on FICLONE** — [GitHub kernel docs](https://github.com/torvalds/linux/blob/master/Documentation/filesystems/btrfs.rst).

- **xfs reflink documentation** — [xfs project docs](https://xfs.org/index.php/Reflink).

## 8. Crates Brain depends on for storage

- **glommio** — async runtime. [GitHub: DataDog/glommio](https://github.com/DataDog/glommio).
- **redb** — embedded ACID key-value store. [GitHub: cberner/redb](https://github.com/cberner/redb).
- **memmap2** — Rust mmap wrapper. [GitHub: RazrFalcon/memmap2-rs](https://github.com/RazrFalcon/memmap2-rs).
- **rkyv** — zero-copy structured serialization. [GitHub: rkyv/rkyv](https://github.com/rkyv/rkyv).
- **bytemuck** — safe bit-cast operations. [GitHub: Lokathor/bytemuck](https://github.com/Lokathor/bytemuck).
- **crc32fast** — CRC32C with hardware acceleration. [GitHub: srijs/rust-crc32fast](https://github.com/srijs/rust-crc32fast).
- **blake3** — fingerprint and snapshot integrity. [GitHub: BLAKE3-team/BLAKE3](https://github.com/BLAKE3-team/BLAKE3).

## 9. Standards

- **RFC 9562** — UUID Formats including UUIDv7. Used for shard and segment identifiers. [datatracker.ietf.org/doc/rfc9562](https://datatracker.ietf.org/doc/rfc9562/).

- **POSIX `fsync`/`fdatasync` semantics** — POSIX.1-2017. The substrate trusts that these primitives behave as documented.
