use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Content, Implementation, ListResourcesResult,
        ListToolsResult, PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams,
        ReadResourceResult, ServerCapabilities, ServerInfo,
    },
    service::{RequestContext, RoleServer},
    tool, tool_router,
    transport::stdio,
};

use super::resources;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::assert::check;
use crate::http::{RequestError, build_client, execute_request};
use crate::parser::load_suite;
use crate::report::render;
use crate::types::{Failure, SuiteResult, TestResult, TestSuite, VarSource};
use crate::variable::{resolve_variables, substitute, substitute_with_check};

const DEFAULT_TIMEOUT_MS: u64 = 30000;
// =============================================================================
// Public entry point
// =============================================================================

pub async fn run() -> anyhow::Result<()> {
    let server = ApiVerifierServer::new();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// =============================================================================
// MCP Server
// =============================================================================

#[derive(Clone)]
struct ApiVerifierServer {
    tool_router: ToolRouter<Self>,
}

impl ApiVerifierServer {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }
}

// =============================================================================
// ServerHandler impl
// =============================================================================

impl ServerHandler for ApiVerifierServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2025_03_26,
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
            server_info: Implementation {
                name: "rest-e2e-mcp".to_string(),
                title: Some("REST E2E MCP — API Verification Server".to_string()),
                description: Some(
                    "Run YAML test suites, assert responses, generate reports. \
                     5 tools: `run`, `parse`, `config`, `help`, `schema`."
                        .to_string(),
                ),
                version: env!("CARGO_PKG_VERSION").to_string(),
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "API verification MCP server.\n\
                 \n\
                 - `run`: Execute .http file or inline request, assert, generate report.\n\
                 - `parse`: Parse .http file without executing (dry-run).\n\
                 - `config`: Show variable resolution with source tracing.\n\
                 - `help`: Show usage overview and YAML format guide.\n\
                 - `schema`: Emit JSON Schema for TestSuite definition."
                    .to_string(),
            ),
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self.tool_router.list_all(),
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let tool_ctx = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tool_ctx).await
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(resources::list_all())
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        resources::read(&request.uri).ok_or_else(|| {
            McpError::invalid_params(
                format!("unknown resource uri: {}", request.uri),
                None,
            )
        })
    }
}

// =============================================================================
// Request types
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct InlineRequest {
    #[schemars(description = "HTTP method (GET, POST, PUT, DELETE, PATCH)")]
    pub method: String,
    #[schemars(description = "Request URL")]
    pub url: String,
    #[schemars(description = "Request headers")]
    pub headers: Option<HashMap<String, String>>,
    #[schemars(description = "Request body")]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct McpRunRequest {
    #[schemars(description = ".http file path")]
    pub file: Option<String>,

    #[schemars(description = "Inline request (alternative to file)")]
    pub request: Option<InlineRequest>,

    #[schemars(
        description = "Filter: '5' (index), '0-5' (range), '0,1,3' (comma), '*csv*' (glob), '@tag:smoke' (tag), 'text' (substring)"
    )]
    pub filter: Option<String>,

    #[schemars(description = ".env file path")]
    pub env_file: Option<String>,

    #[schemars(description = "Variable overrides")]
    pub variables: Option<HashMap<String, String>>,

    #[schemars(description = "Stop on first failure (default: false)")]
    pub stop_on_failure: Option<bool>,

    #[schemars(description = "Delay between requests in ms (default: 0)")]
    pub delay_ms: Option<u64>,

    #[schemars(description = "Request timeout in ms (default: 30000)")]
    pub timeout_ms: Option<u64>,

    #[schemars(description = "Output format: 'markdown', 'json', 'summary' (default: 'summary')")]
    pub format: Option<String>,

    #[schemars(description = "Custom Jinja template path")]
    pub template: Option<String>,

    #[schemars(description = "Max response body lines in report (default: unlimited)")]
    pub body_max_lines: Option<usize>,

    #[schemars(
        description = "Output file path. If not specified, auto-saves to $REST_E2E_SAVE_DIR or current directory."
    )]
    pub output_file: Option<String>,

    #[schemars(description = "Skip writing output file (default: false)")]
    pub no_output_file: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct McpParseRequest {
    #[schemars(description = ".http file path")]
    pub path: String,

    #[schemars(description = "Resolve variables (default: false)")]
    pub resolve_vars: Option<bool>,

    #[schemars(description = ".env file path")]
    pub env_file: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct McpConfigRequest {
    #[schemars(description = ".env file path")]
    pub env_file: Option<String>,

    #[schemars(description = ".http file path (to read @variables)")]
    pub http_file: Option<String>,

    #[schemars(description = "Variable overrides (simulates runtime vars)")]
    pub variables: Option<HashMap<String, String>>,

    #[schemars(description = "Filter variable names (glob pattern)")]
    pub filter: Option<String>,
}

// =============================================================================
// Tool implementations
// =============================================================================

#[tool_router]
impl ApiVerifierServer {
    #[tool(
        name = "run",
        description = "Execute .http file or inline request. Asserts responses against `# 期待:` comments. Returns results and report.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn run(
        &self,
        Parameters(req): Parameters<McpRunRequest>,
    ) -> Result<CallToolResult, McpError> {
        let run_timeout_ms = req.timeout_ms; // None = 未指定（スイートレベルにフォールバック）
        let delay_ms = req.delay_ms.unwrap_or(0);
        let stop_on_failure = req.stop_on_failure.unwrap_or(false);
        let body_max_lines = req.body_max_lines;
        let format = req.format.as_deref().unwrap_or("summary");

        let env_file = req.env_file.as_ref().map(PathBuf::from);
        let runtime_vars = req.variables.unwrap_or_default();

        // 共有HTTPクライアント（コネクションプール再利用）
        let client = build_client().map_err(|e| McpError::internal_error(e.to_string(), None))?;

        // インラインリクエスト（スイートなし → run指定 or デフォルト）
        let inline_timeout = run_timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if let Some(inline) = &req.request {
            let resolved = resolve_variables(&runtime_vars, &HashMap::new(), env_file.as_deref());
            let url = substitute(&inline.url, &resolved.vars);
            let mut headers: HashMap<String, String> = inline
                .headers
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, substitute(&v, &resolved.vars)))
                .collect();

            inject_bearer(&mut headers, &resolved.vars, false);

            let body = inline
                .body
                .as_deref()
                .map(|b| substitute(b, &resolved.vars));

            let response = execute_request(
                &client,
                &inline.method,
                &url,
                &headers,
                body.as_deref(),
                inline_timeout,
            )
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

            let result_json = serde_json::to_string_pretty(&response)
                .map_err(|e| McpError::internal_error(format!("JSON error: {e}"), None))?;

            return Ok(CallToolResult::success(vec![Content::text(result_json)]));
        }

        // YAMLファイル実行
        let file_path = req.file.as_ref().ok_or_else(|| {
            McpError::invalid_params("Either 'file' or 'request' must be provided", None)
        })?;

        let suite =
            load_suite(&PathBuf::from(file_path)).map_err(|e| McpError::internal_error(e, None))?;

        let resolved = resolve_variables(&runtime_vars, &suite.variables, env_file.as_deref());

        // タイムアウト解決: run指定 > YAML suite > デフォルト
        let suite_timeout =
            run_timeout_ms.unwrap_or_else(|| suite.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));

        // フィルタ適用
        let requests = filter_requests(&suite, req.filter.as_deref());

        let suite_start = Instant::now();
        let mut results: Vec<TestResult> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let mut all_unresolved: Vec<String> = Vec::new();

        // Circuit breaker: 同一エラー種別が連続3件で残りをスキップ
        const CIRCUIT_BREAKER_THRESHOLD: usize = 3;
        let mut consecutive_error_keys: Vec<String> = Vec::new();

        for req_def in &requests {
            // skip
            if req_def.skip {
                results.push(TestResult {
                    name: req_def.name.clone(),
                    status: 0,
                    passed: true,
                    elapsed_ms: 0,
                    failures: Vec::new(),
                    response_preview: String::new(),
                    request_method: req_def.method.clone(),
                    request_url: req_def.url.clone(),
                    request_headers: HashMap::new(),
                    request_body: req_def.body.clone(),
                    response_headers: HashMap::new(),
                    response_http_version: String::new(),
                    response_charset: None,
                    response_size_bytes: 0,
                    skipped: true,
                    error_type: None,
                });
                continue;
            }

            // 変数展開（未解決変数を追跡）
            let url_result = substitute_with_check(&req_def.url, &resolved.vars);
            all_unresolved.extend(url_result.unresolved);
            let url = url_result.text;

            let mut headers: HashMap<String, String> = req_def
                .headers
                .iter()
                .map(|(k, v)| {
                    let r = substitute_with_check(v, &resolved.vars);
                    all_unresolved.extend(r.unresolved);
                    (k.clone(), r.text)
                })
                .collect();

            inject_bearer(&mut headers, &resolved.vars, req_def.no_auth);

            let body = req_def.body.as_deref().map(|b| {
                let r = substitute_with_check(b, &resolved.vars);
                all_unresolved.extend(r.unresolved);
                r.text
            });

            let req_timeout = req_def.timeout_ms.unwrap_or(suite_timeout);

            let response = execute_request(
                &client,
                &req_def.method,
                &url,
                &headers,
                body.as_deref(),
                req_timeout,
            )
            .await;

            match response {
                Ok(resp) => {
                    consecutive_error_keys.clear();

                    let failures = check(req_def.expect.as_ref(), &resp);
                    let passed = failures.is_empty();

                    // 実際に送信されたヘッダーで上書き
                    let actual_headers = resp.actual_request_headers;

                    results.push(TestResult {
                        name: req_def.name.clone(),
                        status: resp.status,
                        passed,
                        elapsed_ms: resp.elapsed_ms,
                        failures,
                        response_preview: resp.body,
                        request_method: req_def.method.clone(),
                        request_url: url,
                        request_headers: actual_headers,
                        request_body: body,
                        response_headers: resp.headers,
                        response_http_version: resp.http_version,
                        response_charset: resp.charset,
                        response_size_bytes: resp.size_bytes,
                        skipped: false,
                        error_type: None,
                    });

                    if !passed && stop_on_failure {
                        break;
                    }
                }
                Err(err) => {
                    let error_key = err.error_key().to_string();
                    let elapsed = match &err {
                        RequestError::Timeout { elapsed_ms, .. } => *elapsed_ms,
                        _ => 0,
                    };
                    let err_msg = err.to_string();

                    results.push(TestResult {
                        name: req_def.name.clone(),
                        status: 0,
                        passed: false,
                        elapsed_ms: elapsed,
                        failures: vec![Failure {
                            check: "request".to_string(),
                            expected: "(success)".to_string(),
                            actual: err_msg.clone(),
                        }],
                        response_preview: String::new(),
                        request_method: req_def.method.clone(),
                        request_url: url,
                        request_headers: headers,
                        request_body: body,
                        response_headers: HashMap::new(),
                        response_http_version: String::new(),
                        response_charset: None,
                        response_size_bytes: 0,
                        skipped: false,
                        error_type: Some(error_key.clone()),
                    });

                    // Circuit breaker: 同一エラーが連続N件
                    consecutive_error_keys.push(error_key.clone());
                    if consecutive_error_keys.len() >= CIRCUIT_BREAKER_THRESHOLD
                        && consecutive_error_keys.iter().all(|k| *k == error_key)
                    {
                        let remaining = requests.len() - results.len();
                        if remaining > 0 {
                            warnings.push(format!(
                                "Circuit breaker: {CIRCUIT_BREAKER_THRESHOLD} consecutive '{error_key}' errors — skipped remaining {remaining} requests. Last error: {err_msg}"
                            ));
                        }
                        break;
                    }

                    if stop_on_failure {
                        break;
                    }
                }
            }

            // delay
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        }

        let suite_elapsed = suite_start.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let passed = results.iter().filter(|r| r.passed && !r.skipped).count();
        let failed = results.iter().filter(|r| !r.passed && !r.skipped).count();
        let skipped = results.iter().filter(|r| r.skipped).count();

        // 未解決変数の警告生成
        all_unresolved.sort();
        all_unresolved.dedup();
        if !all_unresolved.is_empty() {
            let var_list = all_unresolved.join(", ");
            warnings.push(format!(
                "Unresolved variables: {var_list} — use `config` tool to check variable resolution"
            ));
        }

        let suite_result = SuiteResult {
            total: results.len(),
            passed,
            failed,
            skipped,
            elapsed_ms: suite_elapsed,
            results,
            warnings,
        };

        // サマリ（MCPレスポンス用 — 常にこれだけ返す）
        let mut summary = format!(
            "{} tests: {} passed, {} failed{} ({}ms)",
            suite_result.total,
            suite_result.passed,
            suite_result.failed,
            if suite_result.skipped > 0 {
                format!(", {} skipped", suite_result.skipped)
            } else {
                String::new()
            },
            suite_result.elapsed_ms,
        );
        // FAIL したテスト名を列挙
        for r in &suite_result.results {
            if !r.passed && !r.skipped {
                summary.push_str(&format!("\n  FAIL: {}", r.name));
            }
        }
        for w in &suite_result.warnings {
            summary.push_str(&format!("\nWARNING: {w}"));
        }

        // ファイル出力（詳細レポート）
        let skip_file = req.no_output_file.unwrap_or(false);
        if !skip_file {
            let out_path = if let Some(ref explicit) = req.output_file {
                PathBuf::from(explicit)
            } else {
                auto_output_path(req.file.as_deref())
            };

            let detail = match format {
                "json" => serde_json::to_string_pretty(&suite_result)
                    .map_err(|e| McpError::internal_error(format!("JSON error: {e}"), None))?,
                _ => {
                    let custom_template = if let Some(tmpl_path) = &req.template {
                        let tmpl = std::fs::read_to_string(tmpl_path).map_err(|e| {
                            McpError::internal_error(format!("Template read error: {e}"), None)
                        })?;
                        Some(tmpl)
                    } else {
                        None
                    };
                    render(&suite_result, custom_template.as_deref(), body_max_lines)
                        .map_err(|e| McpError::internal_error(e, None))?
                }
            };

            if let Some(parent) = out_path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(|e| {
                    McpError::internal_error(
                        format!("Failed to create output directory: {e}"),
                        None,
                    )
                })?;
            }
            // Resolve .. and symlinks to prevent path traversal
            let resolved = resolve_output_path(&out_path)
                .map_err(|e| McpError::internal_error(format!("Invalid output path: {e}"), None))?;
            std::fs::write(&resolved, &detail).map_err(|e| {
                McpError::internal_error(format!("Failed to write output file: {e}"), None)
            })?;
            summary.push_str(&format!("\nReport saved to: {}", resolved.display()));
        }

        let response_text = summary;

        Ok(CallToolResult::success(vec![Content::text(response_text)]))
    }

    #[tool(
        name = "parse",
        description = "Parse .http file without executing. Shows parsed requests, variables, and expected assertions. Useful for dry-run validation.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn parse(
        &self,
        Parameters(req): Parameters<McpParseRequest>,
    ) -> Result<CallToolResult, McpError> {
        let suite =
            load_suite(&PathBuf::from(&req.path)).map_err(|e| McpError::internal_error(e, None))?;

        let resolve = req.resolve_vars.unwrap_or(false);

        if resolve {
            let env_file = req.env_file.as_ref().map(PathBuf::from);
            let resolved =
                resolve_variables(&HashMap::new(), &suite.variables, env_file.as_deref());

            let mut output = format!("Variables ({}):\n", resolved.vars.len());
            for (k, v) in &resolved.vars {
                output.push_str(&format!("  {k} = {v}\n"));
            }
            output.push_str(&format!("\nRequests ({}):\n", suite.requests.len()));
            for (i, r) in suite.requests.iter().enumerate() {
                let url = substitute(&r.url, &resolved.vars);
                let status_info = r
                    .expect
                    .as_ref()
                    .and_then(|e| e.status.as_ref())
                    .map(|s| format!(" (expect: {s})"))
                    .unwrap_or_default();
                output.push_str(&format!(
                    "  [{i}] {}{} {} {}{}\n",
                    if r.skip { "SKIP " } else { "" },
                    r.method,
                    url,
                    r.name,
                    status_info,
                ));
            }

            Ok(CallToolResult::success(vec![Content::text(output)]))
        } else {
            let json = serde_json::to_string_pretty(&suite)
                .map_err(|e| McpError::internal_error(format!("JSON error: {e}"), None))?;
            Ok(CallToolResult::success(vec![Content::text(json)]))
        }
    }

    #[tool(
        name = "config",
        description = "Show variable resolution with source tracing. Displays where each variable comes from and which sources were overridden.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn config(
        &self,
        Parameters(req): Parameters<McpConfigRequest>,
    ) -> Result<CallToolResult, McpError> {
        let runtime_vars = req.variables.unwrap_or_default();
        let env_file = req.env_file.as_ref().map(PathBuf::from);

        // YAMLファイルからの変数
        let suite_vars = if let Some(http_path) = &req.http_file {
            let suite = load_suite(&PathBuf::from(http_path))
                .map_err(|e| McpError::internal_error(e, None))?;
            suite.variables
        } else {
            HashMap::new()
        };

        let resolved = resolve_variables(&runtime_vars, &suite_vars, env_file.as_deref());

        // フィルタ適用
        let traces = if let Some(filter) = &req.filter {
            let pattern = normalize_pattern(filter);
            resolved
                .traces
                .into_iter()
                .filter(|t| matches_simple_pattern(&t.name, &pattern))
                .collect()
        } else {
            resolved.traces
        };

        // Mise風出力
        let mut output = String::new();

        for trace in &traces {
            let chain: Vec<String> = trace
                .found_in
                .iter()
                .map(|(src, val)| {
                    let marker = if *src == trace.source { "here" } else { val };
                    format!("{src}({marker})")
                })
                .collect();

            let display_value = if trace.value.chars().count() > 40 {
                let truncated: String = trace.value.chars().take(37).collect();
                format!("{truncated}...")
            } else {
                trace.value.clone()
            };

            output.push_str(&format!(
                "${:<20} = {:<42} ({})\n",
                trace.name,
                display_value,
                chain.join(" -> ")
            ));
        }

        // Bearer自動注入状態
        let bearer_var = if resolved.vars.contains_key("API_KEY") {
            Some("API_KEY")
        } else if resolved.vars.contains_key("BEARER_TOKEN") {
            Some("BEARER_TOKEN")
        } else {
            None
        };

        output.push('\n');
        if let Some(var) = bearer_var {
            output.push_str(&format!("auto-bearer: ON (from ${var})\n"));
        } else {
            output.push_str("auto-bearer: OFF (no $API_KEY or $BEARER_TOKEN found)\n");
        }

        // ファイルロード情報
        output.push('\n');
        if let Some(env_path) = &req.env_file {
            let count = traces
                .iter()
                .filter(|t| t.found_in.iter().any(|(s, _)| *s == VarSource::DotEnv))
                .count();
            output.push_str(&format!("files: .env={env_path} ({count} vars)\n"));
        }
        if let Some(http_path) = &req.http_file {
            output.push_str(&format!(
                "       suite={http_path} ({} vars)\n",
                suite_vars.len()
            ));
        }

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        name = "help",
        description = "Show usage overview and YAML test suite format guide.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn help(&self) -> Result<CallToolResult, McpError> {
        let help_text = r#"# rest-e2e-mcp — API Verification Server

## Tools

| Tool | Description |
|------|-------------|
| `run` | Execute YAML test suite or inline request, assert responses, generate report |
| `parse` | Parse YAML file without executing (dry-run validation) |
| `config` | Show variable resolution with source tracing |
| `help` | This help message |
| `schema` | Emit JSON Schema for TestSuite YAML definition |

## YAML Test Suite Format

```yaml
variables:
  BASE_URL: https://api.example.com
  ASSET_ID: abc123

requests:
  - name: ヘルスチェック
    method: GET
    url: "{{BASE_URL}}/health"
    expect:
      status: 200

  - name: エクスポート
    method: POST
    url: "{{BASE_URL}}/export"
    headers:
      Content-Type: application/json
    body: '{"assetId": "{{ASSET_ID}}"}'
    timeout_ms: 60000
    tags: [csv, export]
    expect:
      status: 200
      headers:
        content-type: text/csv
      body_contains:
        - 会社名
      body_not_contains:
        - error

  - name: スキップするテスト
    method: GET
    url: "{{BASE_URL}}/skip-me"
    skip: true
```

## Variable Substitution

| Syntax | Description |
|--------|-------------|
| `{{VAR}}` | Template variable |
| `${VAR}` | Braced variable |
| `${VAR:default}` | With default value (braces only) |
| `$VAR` | Bare variable (no default support) |

## Variable Priority (high → low)

1. `runtime` — run tool's `variables` parameter
2. `suite` — YAML file's `variables` section
3. `.env` — .env file
4. `OS` — OS environment variables

## Bearer Auto-Injection

If `$API_KEY` or `$BEARER_TOKEN` is set, `Authorization: Bearer <token>` is automatically added.
Disable per-request with `no_auth: true`.

## Timeout

Timeout priority (high → low):
1. Per-request `timeout_ms` in YAML
2. `run` tool's `timeout_ms` parameter
3. Suite-level `timeout_ms` in YAML
4. Default: 30000ms

```yaml
timeout_ms: 60000  # suite-level default

requests:
  - name: fast endpoint
    method: GET
    url: "{{BASE_URL}}/health"
    timeout_ms: 5000  # per-request override
```

## Status Expect

Single or multiple status codes:
```yaml
expect:
  status: 200        # single
  status: [200, 404] # multiple (any match = pass)
```

## Filter Syntax

| Syntax | Example | Description |
|--------|---------|-------------|
| range | `0-4` | Index 0 to 4 (inclusive) |
| comma | `0,1,3` | Specific indices |
| single | `5` | Single index |
| tag | `@tag:smoke` | Tag-based filter |
| glob | `*csv*` | Name glob match |
| text | `export` | Name substring match |

## Circuit Breaker

If 3 consecutive requests fail with the same error type (timeout, connection, dns, tls), remaining requests are automatically skipped. This prevents wasting time when the server is unreachable.

## Troubleshooting

| Symptom | Likely Cause | Action |
|---------|-------------|--------|
| All requests: `Timeout after Nms` | Server slow or unreachable | Increase `timeout_ms`, check server |
| `{{VAR}}` in error response | Variable unresolved | Use `config` tool to check resolution |
| 401 Unauthorized | Missing or invalid token | Check `$API_KEY` in `.env`, or set `no_auth: true` |
| `Connection error` | Server down or wrong URL | Verify URL, check server status |
| `DNS resolution failed` | Wrong hostname | Check URL hostname |
"#;
        Ok(CallToolResult::success(vec![Content::text(help_text)]))
    }

    #[tool(
        name = "schema",
        description = "Emit JSON Schema for TestSuite YAML definition. Use this to validate your YAML files or generate editor completions.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn schema(&self) -> Result<CallToolResult, McpError> {
        let schema = schemars::generate::SchemaSettings::default()
            .into_generator()
            .into_root_schema_for::<TestSuite>();

        let json = serde_json::to_string_pretty(&schema).map_err(|e| {
            McpError::internal_error(format!("Schema serialization error: {e}"), None)
        })?;

        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

// =============================================================================
// Helper functions
// =============================================================================

/// 出力ファイルパスを自動生成する。
/// 優先順位: $REST_E2E_SAVE_DIR → カレントディレクトリ。
/// ファイル名: {入力ファイルstem}-{YYYYMMDD-HHmmss}-result.md
fn auto_output_path(input_file: Option<&str>) -> PathBuf {
    let dir = std::env::var("REST_E2E_SAVE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));

    let stem = input_file
        .map(|f| {
            std::path::Path::new(f)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("suite")
                .to_string()
        })
        .unwrap_or_else(|| "inline".to_string());

    let now = chrono::Local::now();
    let ts = now.format("%Y%m%d-%H%M%S");

    dir.join(format!("{stem}-{ts}-result.md"))
}

/// Bearer自動注入。
/// API_KEY または BEARER_TOKEN が存在し、Authorizationヘッダーが未設定かつ
/// no_auth でない場合に自動で付与する。
///
/// 戻り値: 注入に使用した変数名（"API_KEY" or "BEARER_TOKEN"）。注入しなかった場合は None。
fn inject_bearer(
    headers: &mut HashMap<String, String>,
    vars: &HashMap<String, String>,
    no_auth: bool,
) -> Option<String> {
    if no_auth {
        return None;
    }

    if headers
        .keys()
        .any(|k| k.eq_ignore_ascii_case("authorization"))
    {
        return None;
    }

    for var_name in &["API_KEY", "BEARER_TOKEN"] {
        if let Some(token) = vars.get(*var_name) {
            headers.insert("Authorization".to_string(), format!("Bearer {token}"));
            return Some(var_name.to_string());
        }
    }
    None
}

/// フィルタ文字列でリクエストを絞り込む。
fn filter_requests(suite: &TestSuite, filter: Option<&str>) -> Vec<crate::types::RequestDef> {
    let requests = &suite.requests;
    let Some(filter) = filter else {
        return requests.clone();
    };

    let filter = filter.trim();
    if filter.is_empty() {
        return requests.clone();
    }

    // @tag: フィルタ
    if let Some(tag) = filter.strip_prefix("@tag:") {
        return requests
            .iter()
            .filter(|r| r.tags.iter().any(|t| t == tag.trim()))
            .cloned()
            .collect();
    }

    // glob フィルタ（名前に対して）
    if filter.contains('*') {
        let pattern = normalize_pattern(filter);
        return requests
            .iter()
            .filter(|r| matches_simple_pattern(&r.name, &pattern))
            .cloned()
            .collect();
    }

    // インデックスベースのフィルタ: "0-5"
    if let Some((start, end)) = filter.split_once('-')
        && let (Ok(s), Ok(e)) = (start.trim().parse::<usize>(), end.trim().parse::<usize>())
    {
        return requests
            .iter()
            .enumerate()
            .filter(|(i, _)| *i >= s && *i <= e)
            .map(|(_, r)| r.clone())
            .collect();
    }

    // カンマ区切りインデックス: "0,1,3"
    if filter.contains(',') {
        let indices: Vec<usize> = filter
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        return requests
            .iter()
            .enumerate()
            .filter(|(i, _)| indices.contains(i))
            .map(|(_, r)| r.clone())
            .collect();
    }

    // 単一インデックス
    if let Ok(idx) = filter.parse::<usize>() {
        return requests.get(idx).into_iter().cloned().collect();
    }

    // フォールバック: 名前部分一致
    requests
        .iter()
        .filter(|r| r.name.contains(filter))
        .cloned()
        .collect()
}

/// パターン文字列を正規化する（小文字化）。
fn normalize_pattern(pattern: &str) -> String {
    pattern.to_lowercase()
}

/// 簡易パターンマッチ（* を含むglob）。
fn matches_simple_pattern(text: &str, pattern: &str) -> bool {
    let text = text.to_lowercase();
    let parts: Vec<&str> = pattern.split('*').collect();

    if parts.len() == 1 {
        return text == pattern;
    }

    let mut pos = 0;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match text[pos..].find(part) {
            Some(found) => {
                if i == 0 && found != 0 {
                    return false;
                }
                pos += found + part.len();
            }
            None => return false,
        }
    }

    if !pattern.ends_with('*')
        && let Some(last) = parts.last()
        && !last.is_empty()
        && !text.ends_with(last)
    {
        return false;
    }

    true
}

/// 出力パスを正規化する（.. やシンボリックリンクを解決）。
fn resolve_output_path(path: &std::path::Path) -> Result<PathBuf, String> {
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| "No filename in output path".to_string())?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|e| format!("Cannot resolve parent directory: {e}"))?;
    Ok(canonical_parent.join(file_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inject_bearer() {
        let mut headers = HashMap::new();
        let vars = HashMap::from([("API_KEY".to_string(), "dummy_token_abc".to_string())]);
        let result = inject_bearer(&mut headers, &vars, false);
        assert_eq!(
            headers.get("Authorization").unwrap(),
            "Bearer dummy_token_abc"
        );
        assert_eq!(result, Some("API_KEY".to_string()));
    }

    #[test]
    fn test_inject_bearer_no_auth() {
        let mut headers = HashMap::new();
        let vars = HashMap::from([("API_KEY".to_string(), "dummy_token_abc".to_string())]);
        let result = inject_bearer(&mut headers, &vars, true);
        assert!(!headers.contains_key("Authorization"));
        assert_eq!(result, None);
    }

    #[test]
    fn test_inject_bearer_already_present() {
        let mut headers =
            HashMap::from([("Authorization".to_string(), "Basic dummy_cred".to_string())]);
        let vars = HashMap::from([("API_KEY".to_string(), "dummy_token_abc".to_string())]);
        let result = inject_bearer(&mut headers, &vars, false);
        assert_eq!(headers.get("Authorization").unwrap(), "Basic dummy_cred");
        assert_eq!(result, None);
    }

    #[test]
    fn test_inject_bearer_uses_bearer_token_fallback() {
        let mut headers = HashMap::new();
        let vars = HashMap::from([("BEARER_TOKEN".to_string(), "dummy_fallback".to_string())]);
        let result = inject_bearer(&mut headers, &vars, false);
        assert_eq!(
            headers.get("Authorization").unwrap(),
            "Bearer dummy_fallback"
        );
        assert_eq!(result, Some("BEARER_TOKEN".to_string()));
    }

    #[test]
    fn test_inject_bearer_no_token_vars() {
        let mut headers = HashMap::new();
        let vars = HashMap::from([("OTHER_VAR".to_string(), "value".to_string())]);
        let result = inject_bearer(&mut headers, &vars, false);
        assert!(!headers.contains_key("Authorization"));
        assert_eq!(result, None);
    }

    #[test]
    fn test_matches_simple_pattern() {
        assert!(matches_simple_pattern("csv_export_test", "*csv*"));
        assert!(matches_simple_pattern("CSV_Export_Test", "*csv*"));
        assert!(!matches_simple_pattern("json_export", "*csv*"));
        assert!(matches_simple_pattern("test_foo", "test_*"));
        assert!(!matches_simple_pattern("my_test_foo", "test_*"));
    }
}
