# TestSuite YAML Format

The authoritative definition lives in `rest-e2e://spec/testsuite.schema.yaml`.
This guide describes the same structure in prose. When the two disagree,
the schema wins.

## TestSuite (top level)

| field        | type                  | required | notes |
|--------------|-----------------------|----------|-------|
| `variables`  | `map<string,string>`  | no       | Suite-scoped variables. Lowest priority source. |
| `timeout_ms` | `integer`             | no       | Default per-request timeout. |
| `requests`   | `RequestDef[]`        | yes      | Ordered list of HTTP requests. |

## RequestDef

| field        | type                          | required | notes |
|--------------|-------------------------------|----------|-------|
| `name`       | `string`                      | yes      | Human-readable label used in reports. |
| `method`     | enum                          | yes      | `GET` / `POST` / `PUT` / `DELETE` / `PATCH` / `HEAD` / `OPTIONS`. |
| `url`        | `string`                      | yes      | Supports `{{VAR}}`, `$VAR`, `${VAR}` interpolation. |
| `headers`    | `map<string,string>`          | no       | Request headers. |
| `body`       | `string`                      | no       | Raw body (JSON, form, etc.). |
| `expect`     | `ExpectDef`                   | no       | Assertions for the response. |
| `no_auth`    | `bool` (default `false`)      | no       | Disable automatic Bearer injection. |
| `timeout_ms` | `integer`                     | no       | Per-request override. |
| `skip`       | `bool` (default `false`)      | no       | Skip this request. |
| `tags`       | `string[]`                    | no       | Consumed by the `@tag:<name>` filter. |

## ExpectDef

| field               | type                 | notes |
|---------------------|----------------------|-------|
| `status`            | `StatusExpect`       | Single integer or list of acceptable codes. |
| `headers`           | `map<string,string>` | Case-insensitive key match, substring value match. |
| `body_contains`     | `string[]`           | Each substring must appear in the body. |
| `body_not_contains` | `string[]`           | None of these substrings may appear in the body. |
| `body_matches`      | `string[]`           | Each regex (Rust regex syntax) must match somewhere in the body. |
| `body_not_matches`  | `string[]`           | None of these regexes may match anywhere in the body. |

## StatusExpect

Either a single integer in the range `100..=599`, or a list of integers
in the same range (matches when the actual status is any one of them).

```yaml
expect:
  status: 200            # single
  status: [200, 201]     # any-of
```

## Editor support

Reference the YAML schema from your suite file to enable completion and
diagnostics in editors with YAML language server support:

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/ynishi/rest-e2e-mcp/main/resources/spec/testsuite.schema.yaml
```
