# Getting Started

`rest-e2e-mcp` runs HTTP end-to-end tests defined in YAML. This guide gets
you to a passing assertion in five minutes.

## 1. Write a suite

Save the following as `smoke.yaml`:

```yaml
variables:
  BASE_URL: "https://httpbin.org"

requests:
  - name: "smoke"
    method: GET
    url: "{{BASE_URL}}/get"
    expect:
      status: 200
      body_contains: ["url"]
```

## 2. Run it through the MCP tool

```
run(file: "smoke.yaml")
```

The `run` tool executes each request, applies the assertions in `expect`,
and returns a Markdown report. To see the full tool reference, call the
`help` tool or read `rest-e2e://guide/yaml-format`.

## 3. Inspect without sending traffic

- `parse(path: "smoke.yaml")` — dry-run; verifies the YAML and reports
  the parsed structure without any HTTP call.
- `config(http_file: "smoke.yaml")` — shows which source each variable
  was resolved from (suite file, `.env`, OS env, or runtime override).

## 4. Where to go next

- `rest-e2e://guide/yaml-format` — full TestSuite YAML structure
- `rest-e2e://guide/variables` — variable sources and precedence
- `rest-e2e://guide/assertions` — `expect` semantics
- `rest-e2e://spec/testsuite.schema.yaml` — authoritative JSON Schema
- `rest-e2e://example/minimal.yaml` — the example used above
- `rest-e2e://example/github-api.yaml` — a multi-request realistic suite
