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
| 1 (low)  | `SuiteFile`  | The `variables:` map in the YAML suite.              |
| 2        | `DotEnv`     | A `.env` file in the current working directory.      |
| 3        | `OsEnv`      | Process environment variables.                       |
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

If a request references a variable that no source provides, the request
fails with a `Failure::UndefinedVariable` entry in the report. Fix it by
defining the variable in any of the four sources.
