# Assertions

Assertions live under the `expect` field of each request. All assertions
are optional; an `expect`-less request passes if the HTTP call itself
succeeds (transport-level), regardless of status code.

## Status code

```yaml
expect:
  status: 200            # exact match
  status: [200, 201]     # any-of match
```

`status` accepts a single integer or a list. The matcher returns true
when the actual response status equals any listed value.

## Headers

```yaml
expect:
  headers:
    content-type: "application/json"
    x-ratelimit-remaining: "0"
```

- Keys are matched **case-insensitively**.
- Values use **substring match** against the response header value, so
  `"application/json"` will match `application/json; charset=utf-8`.
- The assertion passes only when every listed header is present and its
  value contains the expected substring.

## Body substrings

```yaml
expect:
  body_contains:
    - "\"ok\":true"
    - "request_id"
  body_not_contains:
    - "INTERNAL ERROR"
```

- `body_contains` — every listed substring must appear in the body.
- `body_not_contains` — none of the listed substrings may appear.

Both arrays are checked against the raw response body as a string. They
do not parse JSON; combine them with `expect.headers` if you need to
gate on content type first.

## Body regex

```yaml
expect:
  body_matches:
    - "beforeSystemDate=\\d{4}-\\d{2}-\\d{2}"
  body_not_matches:
    - "^ERROR:"
```

- `body_matches` — every listed pattern must match somewhere in the body
  (`Regex::is_match`, not a full-body anchor).
- `body_not_matches` — none of the listed patterns may match anywhere in
  the body.

Patterns use [Rust `regex` crate](https://docs.rs/regex) syntax. They are
validated when the suite file is loaded (`run` / `parse`); an invalid
pattern fails fast with the offending pattern and request name in the
error message, rather than being silently skipped at assertion time.

## Report semantics

When any assertion fails, the request is marked failed in the report and
the specific failure is recorded (e.g. `Status: expected 200, got 500`).
Multiple assertions can fail in the same request; each failure is
reported independently.
