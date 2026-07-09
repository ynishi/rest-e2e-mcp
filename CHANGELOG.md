# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
