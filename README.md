# rest-e2e-mcp

An MCP (Model Context Protocol) server that enables AI agents to safely and efficiently execute REST API end-to-end tests. Designed around **use cases** — each tool maps to a concrete testing workflow.

## Why

AI agents need to verify API behavior, but raw HTTP tools lack structure. `rest-e2e-mcp` provides:

- **Declarative test suites** in YAML — define requests, expectations, and variables in one file
- **Assertion engine** — status codes, headers, body content checks with structured failure reports
- **Variable resolution** — 4-layer priority (runtime > suite > .env > OS), nested variable expansion
- **Encoding-aware** — Shift_JIS, EUC-JP, UTF-8 auto-detection and conversion
- **Safe by design** — read-only parse/config tools, path traversal prevention, no secrets in output

## Tools

| Tool | Purpose |
|------|---------|
| `run` | Execute a YAML test suite or inline request, assert responses, generate a report |
| `parse` | Parse a test suite without executing (dry-run validation) |
| `config` | Show variable resolution with source tracing |
| `help` | Usage overview and YAML test suite format guide |
| `schema` | Emit JSON Schema for the `TestSuite` definition |

## Install

```bash
cargo install rest-e2e-mcp
```

## Quick Start

### 1. Configure as MCP server

Add to your MCP client configuration (e.g., `.mcp.json`):

```json
{
  "mcpServers": {
    "rest-e2e": {
      "command": "rest-e2e-mcp",
      "args": ["--mcp"]
    }
  }
}
```

### 2. Write a test suite

```yaml
variables:
  BASE_URL: https://api.example.com

requests:
  - name: Health check
    method: GET
    url: "{{BASE_URL}}/health"
    expect:
      status: 200

  - name: Create user
    method: POST
    url: "{{BASE_URL}}/users"
    headers:
      Content-Type: application/json
    body: '{"name": "Alice"}'
    expect:
      status: 201
      headers:
        content-type: application/json
      body_contains:
        - Alice
```

### 3. Run via MCP

The AI agent calls the `run` tool:

```json
{
  "file": "tests/api.yaml",
  "env_file": ".env",
  "format": "markdown"
}
```

## Test Suite Format

### Variables

4-layer resolution with nested expansion:

```yaml
variables:
  API_HOST: https://api.example.com
  BASE_URL: "{{API_HOST}}/v1"    # nested reference — resolved automatically
```

Supported syntax: `{{VAR}}`, `${VAR}`, `${VAR:default}`, `$VAR`

Priority: runtime parameters > suite `variables` > `.env` file > OS environment

### Request Options

```yaml
requests:
  - name: Test name
    method: POST
    url: "{{BASE_URL}}/endpoint"
    headers:
      Content-Type: application/json
    body: '{"key": "value"}'
    timeout_ms: 60000       # per-request timeout
    tags: [smoke, api]      # for filtering
    skip: false             # skip this test
    no_auth: false          # disable auto Bearer injection
    expect:
      status: [200, 201]            # single or multiple
      headers:
        content-type: application/json   # partial match
      body_contains:
        - expected string
      body_not_contains:
        - error
```

### Filtering

The `run` tool supports filtering via the `filter` parameter:

- `"5"` — single index
- `"0-5"` — range
- `"0,1,3"` — comma-separated indices
- `"*csv*"` — glob pattern on test name
- `"@tag:smoke"` — tag filter
- `"health"` — substring match on test name

### Auto Bearer Injection

If `API_KEY` or `BEARER_TOKEN` is set in variables, it is automatically injected as an `Authorization: Bearer ...` header. Disable per-request with `no_auth: true`.

## Output Formats

- **`summary`** — one-line pass/fail count (default)
- **`markdown`** — full report with request/response details, failure diagnostics, error grouping
- **`json`** — structured `SuiteResult` for programmatic consumption

Custom Jinja2 templates are supported via the `template` parameter.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.
