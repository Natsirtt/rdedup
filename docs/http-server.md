# HTTP server configuration

Build with `cargo build --release -p rdedup -p rdedup-server`. Start either
`rdedup serve --config server.toml` or `rdedup-server --config server.toml`.
Both commands use the same startup implementation. Create the repository
directory first; an empty directory can then be initialized by a normal client.
The server resolves its own repository setting, independently of `RDEDUP_URI`
and `RDEDUP_DIR`. Relative repository paths are resolved against the server's
working directory. Stop with Ctrl+C (or SIGTERM on Unix) to drain active requests.

The server serves one repository over **plain HTTP**. Exposing it directly is dangerous: bearer tokens and repository contents travel without encryption. Put it behind a trusted TLS reverse proxy, such as Nginx Proxy Manager, and restrict access to the HTTP listener to that proxy. The default listener is `127.0.0.1:8080`.

Select a TOML file with `--config`. Each setting present in the file wins over its environment variable. Missing settings use their environment variable, then the default below. Invalid explicit values fail startup; they do not fall back. A selected file that cannot be read also fails startup. Unknown file keys are errors.

```toml
repository = "/srv/rdedup"
listen = "127.0.0.1:8080"
require_read_credentials = false
lease_lifetime_secs = 300
exclusive_grace_secs = 900
request_capacity = 4096
request_idle_secs = 60
request_retention_secs = 600

[[credentials]]
name = "build-agent"
token = "REPLACE_WITH_64_HEXADECIMAL_CHARACTERS_FROM_32_RANDOM_BYTES"
```

Generate each token from 32 cryptographically random bytes, for example with `openssl rand -hex 32`. Use a different token and name for each credential. The example placeholder is deliberately invalid. Protect the configuration file and environment as credentials. Never put tokens into repository URLs or command-line arguments.

| Setting | Environment variable | Default |
| --- | --- | --- |
| `repository` | `RDEDUP_SERVER_REPOSITORY` | Required |
| `listen` | `RDEDUP_SERVER_LISTEN` | `127.0.0.1:8080` |
| `credentials` | `RDEDUP_SERVER_CREDENTIALS` | Required, nonempty |
| `require_read_credentials` | `RDEDUP_SERVER_REQUIRE_READ_CREDENTIALS` | `false` |
| `lease_lifetime_secs` | `RDEDUP_SERVER_LEASE_LIFETIME_SECS` | `300` |
| `exclusive_grace_secs` | `RDEDUP_SERVER_EXCLUSIVE_GRACE_SECS` | `900` |
| `request_capacity` | `RDEDUP_SERVER_REQUEST_CAPACITY` | `4096` |
| `request_idle_secs` | `RDEDUP_SERVER_REQUEST_IDLE_SECS` | `60` |
| `request_retention_secs` | `RDEDUP_SERVER_REQUEST_RETENTION_SECS` | `600` |

The credentials environment variable is a JSON array of objects containing `name` and `token`; the TOML file uses the array of tables shown above. Token values decode to exactly 32 bytes. Duplicate names or tokens are rejected. Credential values are redacted in diagnostics.

Public reads also allow anonymous shared leases. Enabling `require_read_credentials` requires credentials for reads and shared-lease acquisition. Writes, replacements, deletion, chunk promotion and exclusive leases always require authentication. Credentials have equal repository access; names identify their lease ownership, not different permission roles.

Lease lifetimes and the exclusive grace period are positive durations in seconds. The grace period must be at least the maximum lease lifetime, so announcing an exclusive operation never shortens a granted lease. Once an exclusive request is pending, new shared requests queue behind it. Existing shared holders may renew only to their fixed cutoff. Uploads that lose protection must stop before publishing a name; already published chunks remain reusable when the repository write restarts.

Acquisition request identities make a lost lease-acquisition response recoverable. Queued requests must continue polling before their idle timeout. Terminal results remain available for the configured retention duration; clients must use fresh identities for new acquisitions and must never reuse a terminal identity. The capacity bounds bookkeeping; active grants are not evicted to admit more requests.

Leases are held in memory. Restarting the server invalidates all leases. Filesystem protection is retained during in-flight storage operations, including cleanup, so lease expiration does not permit concurrent destructive access while a file operation still holds the repository.

## Clients

Set `RDEDUP_HTTP_TOKEN` in the client environment, then use a repository URL:

```sh
rdedup --repo https://store.example.test init --encryption none
rdedup --repo https://store.example.test write build-57 --input archive.tar
rdedup --repo https://store.example.test list
rdedup --repo https://store.example.test load build-57 > restored.tar
```

Run `init` only for an empty repository. A library caller can construct
`backends::http::Http` with a URL and a validated `BearerToken` explicitly.
Normal backend URL selection reads `RDEDUP_HTTP_TOKEN`. HTTPS is supported on
the client side; TLS terminates at the proxy. Tokens in URLs, query strings,
URL fragments and HTTP redirects are rejected.

The client chunks and reconstructs archives locally. Each missing chunk is
uploaded in its own request; reads fetch repository objects separately. Request
and response bodies stream at the server, while the existing client backend
holds one object per active worker. Memory depends on chunk size and concurrency,
not archive size. [Publication input and replay](publication.md) describes file
input, explicit stdin spooling and single-attempt streaming stdin.

The initial client has a 10-second connection timeout, 30-second request timeout
and five-minute lease acquisition bound. Idempotent transient requests allow
three attempts with 100/200 ms pauses. Whole-write protection-loss replay has a
separate bound. Very large chunks, unusually slow links or long maintenance
operations may exceed these defaults; proxy timeouts cannot extend client bounds.

## Reverse proxy

For Nginx Proxy Manager, terminate TLS at the proxy and forward HTTP to the
private server address. A proxy in another container cannot reach the server's
loopback address: bind to the private container interface and restrict network
access to the proxy. Do not publish an unrestricted plaintext host port.

The effective Nginx location should preserve the method, URI and Authorization
header and contain these settings:

```nginx
client_max_body_size 0;
proxy_http_version 1.1;
proxy_request_buffering off;
proxy_buffering off;
proxy_cache off;
proxy_intercept_errors off;
proxy_next_upstream off;
```

Disabling request/response buffering preserves streaming; HTTP/1.1 avoids forced
buffering of chunked requests. Leave retry decisions to the client and preserve
the server's problem responses. See the [Nginx proxy directives](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_request_buffering).
The body-size setting removes Nginx's default 1 MiB limit; it applies to each
object, not the complete archive. See [client_max_body_size](https://nginx.org/en/docs/http/ngx_http_core_module.html#client_max_body_size).
Check Nginx Proxy Manager's generated configuration for duplicate or overriding
directives. This deployment recipe has not yet been exercised against an NPM
instance.

## Operational boundaries

Run one server process per repository endpoint. Leases are process-local, so
requests cannot be load-balanced across independent server processes. The server
and local rdedup clients still use filesystem locks for cross-process exclusion;
network-mounted repository guarantees require the investigation in issue #37.
Do not delete or replace the repository lock file while any process can access it.

Interrupted uploads clean up their temporary objects when their owners unwind.
A forced server termination may leave temporary files; these are excluded from
repository object listings and are never published names. Ensure the repository
filesystem has space for in-flight temporary chunks as well as stored objects.
The protocol replaces the old static read-only HTTP layout; upgrade clients and
server together. Native server TLS is a possible future addition.
