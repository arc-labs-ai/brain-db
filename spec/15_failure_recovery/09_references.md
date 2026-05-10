# 15.09 References

References for failure recovery.

## 1. Foundational papers

- **Gray, "Notes on Data Base Operating Systems" (1978).** [microsoft.com/en-us/research/publication](https://www.microsoft.com/en-us/research/publication/notes-on-database-operating-systems/). Classic on transaction recovery.

- **Mohan et al., "ARIES: A Transaction Recovery Method Supporting Fine-Granularity Locking and Partial Rollbacks Using Write-Ahead Logging" (1992).** [doi.org/10.1145/128765.128770](https://doi.org/10.1145/128765.128770). The canonical recovery algorithm.

- **Gray & Reuter, "Transaction Processing: Concepts and Techniques" (1992).** Comprehensive textbook.

## 2. Crash and disaster recovery

- **Bartlett, Gray, Horst, "Fault Tolerance in Tandem Computer Systems" (1986).** Classic HA system design.

- **Schneider, "Implementing Fault-Tolerant Services Using the State Machine Approach" (1990).** [doi.org/10.1145/98163.98167](https://doi.org/10.1145/98163.98167).

- **AWS, "Resilient Architecture Patterns" — [aws.amazon.com/architecture](https://aws.amazon.com/architecture/well-architected/).

## 3. Chaos engineering

- **Basiri et al., "Chaos Engineering" (IEEE Software, 2016).** [ieeexplore.ieee.org/document/7471636](https://ieeexplore.ieee.org/document/7471636). The Netflix paper.

- **Principles of Chaos Engineering** — [principlesofchaos.org](https://principlesofchaos.org/).

- **Chaos Toolkit** — [chaostoolkit.org](https://chaostoolkit.org/).

- **Litmus** — [litmuschaos.io](https://litmuschaos.io/). Kubernetes-native chaos.

## 4. Linux fault injection

- **Linux fault injection framework** — [kernel.org/doc/html/latest/fault-injection](https://www.kernel.org/doc/html/latest/fault-injection/fault-injection.html).

- **`tc` (traffic control)** — for network fault injection.

## 5. Testing tools for Rust

- **`loom`** — [github.com/tokio-rs/loom](https://github.com/tokio-rs/loom). Concurrency model checker.

- **`miri`** — [github.com/rust-lang/miri](https://github.com/rust-lang/miri). Unsafe-code interpreter.

- **`proptest`** — [github.com/proptest-rs/proptest](https://github.com/proptest-rs/proptest). Property-based testing.

## 6. SRE failure response

- **Beyer et al., "Site Reliability Engineering" (SRE Book), Chapter 14: Managing Incidents.** [sre.google/sre-book/managing-incidents](https://sre.google/sre-book/managing-incidents/).

- **Allspaw, "Blameless Postmortems and a Just Culture" (2012).** [codeascraft.com/2012/05/22/blameless-postmortems](https://codeascraft.com/2012/05/22/blameless-postmortems/).

## 7. Backup and snapshot patterns

- **Patterson, Gibson, Katz, "A Case for Redundant Arrays of Inexpensive Disks (RAID)" (1988).** Classic on disk redundancy.

- **PostgreSQL's WAL and backup** — [postgresql.org/docs/current/backup.html](https://www.postgresql.org/docs/current/backup.html).

## 8. Filesystem reflinks

- **xfs reflink documentation** — [xfs.org/index.php/Reflink_Support](https://xfs.org/index.php/Reflink_Support).

- **btrfs reflink** — [btrfs.wiki.kernel.org/index.php/Subvolume](https://btrfs.wiki.kernel.org/index.php/Subvolume).

## 9. Data integrity

- **CRC32C** — [tools.ietf.org/html/rfc3720](https://tools.ietf.org/html/rfc3720) (iSCSI's CRC32C).

- **BLAKE3** — [github.com/BLAKE3-team/BLAKE3](https://github.com/BLAKE3-team/BLAKE3).

- **End-to-end argument in system design** (Saltzer, Reed, Clark, 1984). Classic on where to place reliability checks.

## 10. Brain-internal references

- See [05.06 Recovery](../05_storage_arena_wal/06_recovery.md) for the WAL recovery details.
- See [05.10 Snapshots](../05_storage_arena_wal/10_snapshots.md) for the snapshot mechanism.
- See [11. Background Workers](../11_background_workers/) for worker failure modes.
- See [14. Observability + Operations](../14_observability_ops/) for monitoring failure modes.
