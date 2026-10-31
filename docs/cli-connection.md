# CLI connection and authentication

The CLI and web app use the same account API. The CLI logs in with the account,
uses the access token to create a scoped device key, revokes the refresh token,
and stores the device key locally. The server URL saved in the CLI is the API
base, not necessarily the web page URL.

## Quick start

Install the CLI using the commands in the [README](../README.md), then:

```sh
# Same-origin web deployment: use the API base directly (also works on v0.1.12).
cormex login --server https://memory.example.com/api
cormex init
```

The standard proxy sends `/api/*` to cormexd with `/api` stripped. Its explicit
API base is `https://memory.example.com/api`. A standalone cormexd exposed at
the origin uses `https://memory.example.com`; a custom proxy path must be passed
explicitly, for example `https://memory.example.com/custom/api`.

For a device-code login, use `cormex login --server https://memory.example.com/api
--device-code`. An already issued device key can be supplied with `--key`.
CLI builds with discovery support also accept the website origin without `/api`.

## Discovery contract

When `--server` has no path, the CLI first requests
`GET /.well-known/cormex` on that origin. A web deployment may return JSON:

```json
{"api_base":"/api"}
```

The value must be an absolute path on the same origin; URLs, network-path
references, query strings, fragments, percent escapes and dot segments are not
accepted. The stock web image publishes this document. Deployments using a
custom prefix should publish their own discovery document at the edge or have
users pass the API base explicitly. Discovery does not follow redirects.

For older deployments, the CLI probes `/health` and `/api/health`, in that
order, accepting JSON that identifies `role` as `cormexd` and includes `status`
and `version`. These GET requests carry no credentials. If no discovery or
recognized health response exists, the CLI preserves the supplied origin and
prints a hint; explicit paths are never discovered or rewritten. This keeps
older direct servers usable without treating an arbitrary HTML page as an API.

If login returns 405 or an HTML response, check that `--server` is the API base
and the reverse proxy routes that prefix to cormexd. A successful discovery
does not itself verify an account password or device-key issuance.
