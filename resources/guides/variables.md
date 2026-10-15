# Variables

## Syntax

Three interpolation forms are accepted anywhere variable substitution is
applied (URL, headers, body):

- `{{VAR}}` — Handlebars-style; recommended default.
- `${VAR}` — POSIX-style with braces.
- `$VAR`   — Bare POSIX-style.

All three resolve against the same variable set.

## The four sources

Variables are merged from four sources. Later sources override earlier
ones for the same name:

| priority | source       | where it comes from                                  |
|----------|--------------|------------------------------------------------------|
| 1 (low)  | `OsEnv`      | Process environment variables.                       |
| 2        | `DotEnv`     | The `.env` file given as `env_file`.                 |
| 3        | `SuiteFile`  | The `variables:` map in the YAML suite.              |
| 4 (high) | `Runtime`    | The `variables` parameter of the `run` MCP tool.     |

So a value passed to `run(variables: { TOKEN: "x" })` will always win
over `.env` and the suite's own `variables`.

## Tracing where a value came from

Use the `config` tool to inspect resolution:

```
config(http_file: "smoke.yaml", env_file: ".env")
```

The output lists each variable, the value used, the winning source, and
every source where the name was found. That makes it easy to debug
"why is this BASE_URL hitting production".

## Failures

A reference to a variable that no source provides is not an error: the
placeholder is left unexpanded in the request and the report gets an
`Unresolved variables: ...` warning (use `config` to check resolution). Fix it
by defining the variable in any of the four sources. (A variable whose secret
reference failed to resolve is different: see *Failure behavior* below.)

## Secret references

A variable value may be a **reference** to a secret in an external store
instead of the secret itself. The reference is resolved at run time, once per
call (no caching across calls), and only for the variable's winning value.

| form                                | resolved by                                                          |
|-------------------------------------|----------------------------------------------------------------------|
| `keychain://<service>/<account>`    | macOS: `security find-generic-password -s <service> -a <account> -w` |
| `op://<vault>/<item>/<field>`       | 1Password CLI: `op read op://<vault>/<item>/<field>`                 |

Example `.env`:

```dotenv
BASE_URL=https://api.example.com
API_KEY=keychain://my-api-tests/API_KEY
```

The suite stays unchanged: `{{API_KEY}}` and automatic bearer injection use
the resolved value. The reference itself is not a secret; only the resolved
value is.

### Allowlist: `--secret-ref-allow`

The suite file, the `run` `variables` parameter and the `env_file` path are all
chosen by the AI agent, so no *source* can be trusted. Trust is decided by the
server command line instead:

```
rest-e2e-mcp --mcp --secret-ref-allow 'keychain://my-api-tests/*' --secret-ref-allow 'op://Dev/*'
```

- The flag is repeatable. A pattern is an exact reference or contains `*`,
  which matches any characters including `/`.
- A reference is resolved only if it matches a pattern, regardless of where it
  came from (`.env`, suite `variables`, runtime `variables`). A reference that
  exists only in the OS environment is never reached (see below).
- At startup the server prints the effective allowlist to stderr and warns
  about patterns that are exactly `*` or do not start with `keychain://` /
  `op://`, because such patterns allow references across schemes.
- With no flag, no reference is resolved. A reference that is not allowed fails
  with `SecretResolutionFailed` (reason `not allowed by --secret-ref-allow`);
  the backend is not called and the literal reference is never sent.
- Each backend CLI call is limited to 10 seconds (the whole process group is
  killed on timeout); a timeout is a resolution failure. All references of one
  call share an overall 30 second budget: references not resolved before it
  expires fail with `overall secret resolution deadline exceeded` and their
  backend is not called.

### Where references can come from

The OS environment is not enumerated: it is consulted only for names that also
appear in runtime `variables`, the suite or the `.env` file, and it has the
lowest priority. A reference that exists only in the OS environment is
therefore never reached; put it in the `.env` file (or the suite) instead.
The `parse` tool never runs secret CLIs: with `resolve_vars` it shows
references unresolved (marked as references).

### Failure behavior

If a reference cannot be resolved (CLI not installed, item missing, access
denied, non-zero exit, empty output), the requests that use that variable
fail with `SecretResolutionFailed` (variable name, reference and reason). A
variable whose resolution failed is never replaced by a `${NAME:default}`
default: any request that mentions it, in any form, is blocked. With
`API_KEY` / `BEARER_TOKEN`, every request that would receive the automatic
`Authorization` header fails when the variable auto-bearer would pick (`API_KEY`
first, else `BEARER_TOKEN`) failed; there is no silent fallback from a failed
`API_KEY` to `BEARER_TOKEN`. `config` shows `auto-bearer: BLOCKED (...)` then. There is no fallback to the literal reference or
to an empty string, and the error never contains the resolved value or CLI
output. Suites that use no references never invoke these CLIs.

### Masking

Secret values are masked in everything returned to the MCP client or written
to a report (summary, markdown, json and custom templates are all fed already
masked data):

- A variable is secret if it was resolved from a reference, or if its name is
  `API_KEY` or `BEARER_TOKEN` (regardless of origin).
- The `Authorization` header value is always replaced with `***`
  (case-insensitive header name).
- Any substring equal to a secret value of **8 or more characters** is replaced
  with `***` in headers, URL, request body, response headers/body and failure
  messages, and so are its percent-encoded forms (full percent encoding, the
  `application/x-www-form-urlencoded` form, and the partial encoding the URL
  parser applies to query/path/fragment, which leaves e.g. `/`, `+`, `=` raw) and its JSON-escaped form. Shorter secret values are
  not substring-masked. Other transformed forms (base64, hex, hashes, partial
  substrings, ...) are **not** masked.
- A reference string itself is not a secret and is shown as is.
- `config` shows `<reference> -> <resolved, N chars>` for referenced
  variables and `***` for other secret variables, in the value column and in
  the source chain. `parse` with `resolve_vars` masks secret values too.
