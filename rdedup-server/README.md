# `rdedup serve`

`rdedup serve` exposes a local repository through the rdedup HTTP protocol.
It supports the same repository operations as the HTTP backend, including
uploads, reads, deletion, garbage collection, and reader/writer leases.

## Transport security

The server speaks **plain HTTP**. Do not expose its listener directly to the
internet or an untrusted network. Put it behind a trusted reverse proxy that
terminates TLS, such as Nginx Proxy Manager, and restrict access to the
upstream listener to that proxy. Bearer tokens do not encrypt traffic; without
TLS, anyone able to observe the connection can steal credentials and
repository data.

The default listener is `127.0.0.1:8080`. When the proxy runs in another
container, bind to the server's private container interface and keep the
container network private. Do not publish the rdedup port to the host or
internet unless the network policy permits only the trusted proxy to reach it.

## Configuration

Pass a TOML file with `--config`. For each setting, a file value takes
precedence over its environment variable; environment variables are used when
the file omits that setting.

```toml
repository_path = "/var/lib/rdedup"
bind_address = "0.0.0.0:8080"
lease_ttl_seconds = 300
lease_renewal_grace_seconds = 21600
lease_request_ttl_seconds = 86400

[auth]
require_read_auth = false

[auth.tokens]
build-agent = "replace-with-at-least-32-random-bytes"
artist-workstation = "use-a-different-token-for-each-client"
```

Generate a high-entropy token for each client, for example with
`openssl rand -hex 32`. Treat the configuration file as secret material.
Reads are public by default; set `require_read_auth = true` when clients must
authenticate to read. Writes, deletes, exclusive leases, and other destructive
operations always require a configured token. With public reads enabled, a
shared read lease can be acquired without a token.

Settings can also come from the environment:

| Setting | Environment variable |
| --- | --- |
| Repository path | `RDEDUP_SERVER_REPOSITORY_PATH` |
| Bind address | `RDEDUP_SERVER_BIND_ADDRESS` |
| Lease lifetime in seconds | `RDEDUP_SERVER_LEASE_TTL_SECONDS` |
| Renewal grace in seconds | `RDEDUP_SERVER_LEASE_RENEWAL_GRACE_SECONDS` |
| Lease request lifetime in seconds | `RDEDUP_SERVER_LEASE_REQUEST_TTL_SECONDS` |
| Require read authentication | `RDEDUP_SERVER_REQUIRE_READ_AUTH` |
| Named tokens as comma-separated `name=token` entries | `RDEDUP_SERVER_TOKENS` |

## Run

```sh
rdedup serve --config /etc/rdedup/server.toml
```

The server keeps leases in memory. Restarting it drops every lease and queued
request; clients must retry interrupted operations. Configure the reverse
proxy to accept request bodies up to 1 GiB for repositories with large chunks.
At startup, the server warns when the repository's configured chunk size is
at least 16 MiB because one chunk is buffered for each active upload.
