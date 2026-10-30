//! Copy-on-write file cloning via the Linux `FICLONE` ioctl, with a
//! full-copy fallback.
//!
//! Snapshots clone `arena.bin`, `metadata.redb`, and each WAL segment.
//! On reflink-capable filesystems (btrfs, xfs with `reflink=1`) the
//! clone shares disk blocks copy-on-write — a multi-gigabyte arena is
//! captured in milliseconds with no extra space until one side is
//! written. On filesystems without reflink support (ext4) the operation
//! degrades to a byte-for-byte copy.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};

// `FICLONE` is `_IOW(0x94, 9, int)`. The third ioctl argument is the
// source file descriptor (an `int`). The numeric constant is stable in
// the kernel UAPI (`include/uapi/linux/fs.h`) across architectures Brain
// targets, so we encode it directly rather than depend on a crate.
const FICLONE: libc::c_ulong = 0x4004_9409;

/// The `errno` of the last fallback reported at `warn`. Zero means none yet;
/// `errno` is never zero on a failure, so it is a safe "unset".
static REPORTED_ERRNO: AtomicI32 = AtomicI32::new(0);

/// Whether this `errno` is a cause not already reported, latching it as
/// reported.
///
/// Why a latch at all: on a filesystem without reflink support the ioctl
/// cannot ever succeed, so warning per call emits the same unactionable line
/// for every file of every snapshot, forever — on an hourly snapshot of four
/// files that is ~96 lines a day whose only effect is to teach operators that
/// `WARN` on this service means nothing.
///
/// Why keyed on `errno` rather than a plain "warned once" flag: `EOPNOTSUPP`
/// (the filesystem has no reflinks) and `EXDEV` (`src` and `dst` landed on
/// different mounts) are different problems, and the second is a real
/// misconfiguration worth seeing. A single flag would hide whichever arrived
/// second.
///
/// Relaxed ordering: two threads racing on the same `errno` may both report,
/// which costs one duplicate line and nothing else.
fn first_report_of(seen: &AtomicI32, errno: i32) -> bool {
    seen.swap(errno, Ordering::Relaxed) != errno
}

/// Clone `src` into a freshly created `dst` using `FICLONE`, falling
/// back to a full `std::fs::copy` if the filesystem doesn't support
/// reflinks.
///
/// `dst` is created (truncating any existing file). The first fallback from
/// each distinct cause logs a `tracing::warn!` so operators on ext4 see why a
/// snapshot consumed full disk space; later fallbacks from that same cause log
/// at `debug`, since the filesystem's capabilities do not change between
/// calls.
///
/// # Errors
///
/// Returns the underlying `io::Error` if neither the reflink nor the
/// copy fallback can complete (e.g. the source is missing, or the
/// destination directory isn't writable).
pub fn reflink_or_copy(src: &Path, dst: &Path) -> io::Result<()> {
    let src_file = File::open(src)?;
    let dst_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;

    // SAFETY: `ioctl` with FICLONE reads the source fd (an `int`) and
    // writes into the destination fd's inode. Both fds are owned by the
    // `File` values above and stay alive for the whole call; the kernel
    // does not retain them past the syscall. No Rust-side memory is
    // aliased or mutated.
    let rc = unsafe {
        libc::ioctl(
            dst_file.as_raw_fd(),
            FICLONE,
            src_file.as_raw_fd() as libc::c_int,
        )
    };

    if rc == 0 {
        return Ok(());
    }

    // Reflink failed — almost always EOPNOTSUPP / EXDEV (filesystem
    // doesn't support reflinks, or src and dst are on different mounts).
    // Fall back to a plain copy so the snapshot still completes.
    let err = io::Error::last_os_error();
    if first_report_of(&REPORTED_ERRNO, err.raw_os_error().unwrap_or(-1)) {
        tracing::warn!(
            src = %src.display(),
            dst = %dst.display(),
            error = %err,
            "FICLONE reflink unsupported; falling back to full copy \
             (further fallbacks from this cause log at debug)"
        );
    } else {
        tracing::debug!(
            src = %src.display(),
            dst = %dst.display(),
            error = %err,
            "FICLONE reflink unsupported; falling back to full copy"
        );
    }
    drop(dst_file);
    std::fs::copy(src, dst)?;
    Ok(())
}

// Tests touch real files (`copy_file_range`/`reflink`). Gated out under miri.
#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reflink_or_copy_round_trips_contents() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        let payload = b"reflink-or-copy round-trip payload";
        {
            let mut f = File::create(&src).unwrap();
            f.write_all(payload).unwrap();
            f.sync_all().unwrap();
        }

        reflink_or_copy(&src, &dst).expect("reflink_or_copy");

        assert_eq!(std::fs::read(&dst).unwrap(), payload);
    }

    #[test]
    fn reflink_or_copy_truncates_existing_dst() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        std::fs::write(&src, b"short").unwrap();
        std::fs::write(&dst, b"a-much-longer-pre-existing-file").unwrap();

        reflink_or_copy(&src, &dst).expect("reflink_or_copy");

        assert_eq!(std::fs::read(&dst).unwrap(), b"short");
    }

    #[test]
    fn reflink_or_copy_missing_src_errors() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("does-not-exist.bin");
        let dst = dir.path().join("dst.bin");
        assert!(reflink_or_copy(&src, &dst).is_err());
    }

    // Uses a local latch rather than REPORTED_ERRNO so the assertions don't
    // depend on whether another test in this binary already warned.
    #[test]
    fn repeated_same_cause_reports_once() {
        let seen = AtomicI32::new(0);
        assert!(first_report_of(&seen, libc::EOPNOTSUPP));
        assert!(!first_report_of(&seen, libc::EOPNOTSUPP));
        assert!(!first_report_of(&seen, libc::EOPNOTSUPP));
    }

    #[test]
    fn a_different_cause_is_still_reported() {
        let seen = AtomicI32::new(0);
        assert!(first_report_of(&seen, libc::EOPNOTSUPP));
        assert!(first_report_of(&seen, libc::EXDEV));
        assert!(!first_report_of(&seen, libc::EXDEV));
    }
}
