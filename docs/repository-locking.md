# Repository protection and filesystem evidence

Every local operation opens the repository's `.lock` file and retains an OS lock
through its operation owner. Reads and additive writes share protection; GC,
removal and replacement require exclusive protection. Workers, directory
iterators and object streams retain the same owner. Finishing the initiating
call cannot release protection while one of those resources remains alive.

HTTP leases add admission order and expiration around these local locks. They
do not replace them. A server and a local client therefore coordinate through
the same lock file. Independent HTTP server processes do not share lease IDs:
run one process for each HTTP endpoint, without load-balancing requests across
separate coordinators.

## Lock-file identity

Keep the repository directory and `.lock` file stable while any process can use
them. Do not delete, replace or restore `.lock` underneath active operations.
A lock belongs to the opened filesystem object, not the spelling of a pathname;
replacing that object can let later clients acquire a different lock. Quiesce
all clients before replacing or relocating a repository directory.

The lock is a cooperation protocol. Programs that edit repository files without
using it are outside that protocol. A process crash releases its handles; an
in-process worker still owning a handle intentionally keeps protection alive.
Crash release does not roll back published objects or eliminate abandoned
temporary files.

## Reproduce local process checks

```sh
cargo test --test repository_lock
```

The parent launches a separate copy of the integration-test executable for each
holder and waits for an acquisition acknowledgement before probing exclusion.
Tests cover concurrent shared holders, exclusive exclusion, normal release,
forced termination, retained workers and retained streams. Unix also tests a
symbolic repository alias. The ignored `lock_holder_process` function is an
entry point invoked by those tests, not a standalone test to run manually.

Windows and Linux (Docker) process checks passed during the 2026-10-13
investigation. These are local-filesystem checks; they do not
establish cross-host or network-mount behavior.

## Standard-library option

Rust 1.89 introduced `File::lock`, `lock_shared`, `try_lock` and
`try_lock_shared`. The try methods distinguish contention through
`TryLockError::WouldBlock`. This could replace the current fs2 dependency and
raw OS error-code comparisons without changing operation ownership. The
standard library currently uses `flock` on Unix and `LockFileEx` on Windows;
changing Rust wrappers does not make filesystem locking distributed or stronger.
See the [standard-library locking API](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock).

Before a migration, test interoperability with existing holders and preserve
separate opens per independent operation. Do not acquire a second lock through
a cloned handle or silently turn acquisition errors into permission to proceed.

## Network-mounted repositories

Network support remains unverified in issue #37. The exact client OS, kernel,
server, protocol version and mount options matter. Linux's `flock` behavior on
CIFS changed across kernel versions; NFS can be configured to keep locking
local. A successful same-host probe cannot show that another host sees the
lock. See the [Linux flock documentation](https://man7.org/linux/man-pages/man2/flock.2.html).
Windows documents SMB support for its locking primitive, but that alone does
not verify an rdedup deployment's mounts, interruption behavior or durability.
See [LockFileEx](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-lockfileex).

Any support claim needs two independent hosts accessing the same repository:
shared/shared admission, shared/exclusive and exclusive/exclusive exclusion,
normal release, process termination, server restart and disconnect/reconnect.
Record filesystem identity and mount settings with the results. Also test
atomic object publication and promotion on that filesystem: locking evidence
alone does not prove its hard-link, rename or durability behavior.

Using HTTP clients with server-local repository storage avoids requiring client
hosts to coordinate through a network-mounted filesystem. A repository mounted
from a network filesystem on the HTTP server still needs filesystem evidence.
No automatic mount rejection or generic network-support promise is implemented.
