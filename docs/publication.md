# Publication input and replay

`store` and its alias `write` publish data under a name. An equivalent existing
name (the same digest and index level) succeeds; different content is a conflict.

Choose the source according to whether the complete input can be read again:

```sh
rdedup --repo http://localhost:8080 store build-57 --input archive.tar
tar -cf - artifacts | rdedup --repo http://localhost:8080 store build-57 --spool
tar -cf - artifacts | rdedup --repo http://localhost:8080 store build-57
```

* `--input FILE` opens the file from the beginning for each attempt. Keep the file
  unchanged for the duration of the command. No archive-sized copy is made.
* `--spool` copies all stdin to a temporary file before acquiring the publication
  lease. It requires temporary disk space for the full input and adds a complete
  disk write/read. The temporary file is removed when the command unwinds or
  exits normally; a forced process termination can leave it behind.
* Ordinary stdin streams directly in one attempt. If the lease is lost, the
  command reports interruption and cannot replay the consumed stream.

Replayable inputs permit up to three complete attempts. A retry acquires a new
shared lease, rereads repository generations and reopens the input. It can reuse
chunks that remain in the repository. Authentication failures, name conflicts,
invalid protocol responses and ordinary source/I/O failures do not cause a
whole-publication retry. GC may run between attempts.

Only a complete successful write publishes its name. A lost acknowledgement can
mean a complete publication already succeeded; repeating the same input and name
confirms its identity. An interrupted write does not publish a partial archive.

Library callers use `Repo::write_replayable`, a `ReplayableSource` (or a factory
closure returning a fresh reader), and `WriteRetryPolicy`. Every reader must yield
the same complete byte sequence. `Repo::write` remains the single-attempt API for
readers that cannot be recreated.
