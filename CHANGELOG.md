# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] — 2026-10-15

### Added

- Secret references (#1): a variable value may be `keychain://<service>/<account>`
  (macOS `security`) or `op://<vault>/<item>/<field>` (1Password `op`). A
  reference is resolved at run time only if it matches a server-side
  `--secret-ref-allow <pattern>` flag (repeatable, `*` wildcard); with no flag
  nothing is resolved. Sources (`.env`, suite `variables`, runtime `variables`)
  are not trusted. At startup the effective allowlist is printed to stderr,
  with a warning for patterns that are `*` or lack a known scheme prefix.
- Failure handling: a reference that is not allowed or cannot be resolved fails
  the requests that use the variable with `SecretResolutionFailed` (also for
  `${NAME:default}`), never falling back to the literal reference or a default.
  Each CLI call has a 10 s timeout (process group killed on timeout or oversized output) and a 64 KiB output cap, and all references of
  one call share a 30 s budget.
- Output masking: resolved secrets and the values of `API_KEY` /
  `BEARER_TOKEN` are replaced with `***` (secrets of 8+ characters, in raw,
  percent-encoded, form-encoded, URL-parser-encoded (query, path, fragment,
  userinfo) and JSON-escaped forms)
  in reports, `config`, errors and responses. `Authorization` header values are
  always masked. Header names are not masked.
- `config` shows references and resolved length (never the value) and
  `auto-bearer: BLOCKED (...)` when the selected auto-bearer variable failed;
  `parse` never runs secret CLIs.
- `libc` dependency (unix only) to kill the secret CLI process group.
- Rust API: new `secret` module; `ResolveMode`, `resolve_variables_with`,
  `resolve_variables_in` (backend, allowlist and time budget injectable) and
  `substitute_with_check_failed` are exported from `variable`. New public type
  `ResolveFailure` (`SecretResolutionFailed`).

### Changed

- Report and `config` output now mask `Authorization` and literal `API_KEY` /
  `BEARER_TOKEN` values; output that previously contained them shows `***`.
- A variable whose secret resolution failed is reported once, as
  `SecretResolutionFailed`, not additionally in the `Unresolved variables`
  warning.
- Auto-bearer does not fall back from a failed `API_KEY` to `BEARER_TOKEN`; the
  request fails instead.

### Breaking (Rust API)

- `rest_e2e_mcp::mcp::run` now takes `secret_ref_allow: Vec<String>`.
- `VarTrace` gained the public fields `secret` and `reference`; `ResolvedVars`
  gained `failures`.
- `resolve_variables` keeps its signature but now yields `failures` for secret
  references (the default allowlist is empty, so references fail); use
  `resolve_variables_with` / `resolve_variables_in` to inject a backend and
  allowlist.

## [0.3.0] — 2026-07-09

### Added

- `expect.body_matches` / `expect.body_not_matches`: regex-based body
  assertions (Rust `regex` syntax, matched against the whole response
  body). Useful for variable values such as timestamps, e.g.
  `body_matches: ['beforeSystemDate=\d{4}-\d{2}-\d{2}']`. Patterns are
  validated at suite parse time — an invalid pattern fails parsing with
  an explicit error carrying the request name and the offending
  pattern. Reflected in the `schema` output, the `help` guide, the
  bundled guides (`assertions`, `yaml-format`), the TestSuite schema
  spec, and the `github-api` example.

### Changed

- README now documents the `body_max_lines` default for `run`: when
  unset, `response_preview` in reports contains the full response body;
  apparent truncation typically comes from the MCP client display, and
  `body_max_lines` can be set to control line count explicitly.
- `Cargo.toml` now declares the `documentation` metadata field
  (docs.rs).

## [0.2.0] — 2026-06-27

### Added

- MCP `resources` capability. Static guides, the authoritative TestSuite
  schema, and runnable example suites are now discoverable via
  `resources/list` and readable via `resources/read` without invoking a
  tool. Seven entries are exposed under the `rest-e2e://` URI scheme:
  - `rest-e2e://guide/getting-started`
  - `rest-e2e://guide/yaml-format`
  - `rest-e2e://guide/variables`
  - `rest-e2e://guide/assertions`
  - `rest-e2e://spec/testsuite.schema.yaml`
  - `rest-e2e://example/minimal.yaml`
  - `rest-e2e://example/github-api.yaml`
- A local `justfile` with allow-agent recipes (`test`, `package-list`,
  `publish-dry-run`) for development verification.

### Fixed

- `src/report/default.md.j2` is now included in the published crate. In
  v0.1.1 the template was excluded from the `.crate` tarball, which
  caused consumer builds (e.g. `cargo install rest-e2e-mcp`) to fail at
  compile time on the `include_str!` macro in the report engine. The
  fix adopts an explicit `[package].include` allowlist in `Cargo.toml`
  so that template assets, embedded resources, and source files are
  intentionally listed rather than relying on VCS tracking.

## [0.1.1] — earlier

### Added

- Auto-save result files for `run`.
- New `no_output_file` flag on `run` to suppress automatic saving.

### Fixed

- `body_max_lines` truncation in report rendering.

### Known issues

- `src/report/default.md.j2` is missing from the published `.crate`
  tarball; consumer builds fail. Fixed in 0.2.0.

## [0.1.0] — initial release

- First publication of `rest-e2e-mcp`: an MCP server for AI-driven REST
  API end-to-end testing. Ships five tools (`run`, `parse`, `config`,
  `help`, `schema`) and supports YAML test suites with variable
  interpolation, response assertions, and report generation.
