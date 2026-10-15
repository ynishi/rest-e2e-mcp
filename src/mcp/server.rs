use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
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
use crate::secret::{CliBackend, MASK, Masker, SecretAllowlist, SecretBackend, is_reference};
use crate::types::{
    Failure, ResolveFailure, ResolvedVars, SuiteResult, TestResult, TestSuite, VarSource, VarTrace,
};
use crate::variable::{
    ResolveMode, resolve_variables_in, substitute, substitute_with_check_failed,
};

const DEFAULT_TIMEOUT_MS: u64 = 30000;
// =============================================================================
// Public entry point
// =============================================================================

/// Start the MCP server. `secret_ref_allow` holds the `--secret-ref-allow`
/// patterns: secret references are resolved only if they match one of them.
pub async fn run(secret_ref_allow: Vec<String>) -> anyhow::Result<()> {
    // stderr only: stdout is the MCP stdio transport.
    for line in crate::secret::allowlist_report(&secret_ref_allow) {
        eprintln!("{line}");
    }
    let server = ApiVerifierServer::new(SecretCtx::real(SecretAllowlist::new(secret_ref_allow)));
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// =============================================================================
// MCP Server
// =============================================================================

type OsEnvFn = dyn Fn(&str) -> Option<String> + Send + Sync;

/// Everything variable resolution needs besides the request itself.
#[derive(Clone)]
struct SecretCtx {
    allow: Arc<SecretAllowlist>,
    backend: Arc<dyn SecretBackend>,
    os_env: Arc<OsEnvFn>,
    /// Overall time budget for resolving all references of one call.
    budget: std::time::Duration,
}

impl SecretCtx {
    fn real(allow: SecretAllowlist) -> Self {
        Self {
            allow: Arc::new(allow),
            backend: Arc::new(CliBackend::default()),
            os_env: Arc::new(|n| std::env::var(n).ok()),
            budget: crate::secret::RESOLVE_DEADLINE,
        }
    }
}

#[derive(Clone)]
struct ApiVerifierServer {
    tool_router: ToolRouter<Self>,
    secrets: SecretCtx,
}

impl ApiVerifierServer {
    fn new(secrets: SecretCtx) -> Self {
        Self {
            tool_router: Self::tool_router(),
            secrets,
        }
    }

    /// Resolve variables. Secret CLIs may block, so this runs on the blocking pool
    /// (each CLI call is additionally bounded by a timeout). `ReferencesAsIs`
    /// never invokes a backend and runs inline.
    async fn resolve(
        &self,
        runtime_vars: HashMap<String, String>,
        suite_vars: HashMap<String, String>,
        env_file: Option<PathBuf>,
        mode: ResolveMode,
    ) -> Result<ResolvedVars, McpError> {
        let ctx = self.secrets.clone();
        let work = move || {
            resolve_variables_in(
                &runtime_vars,
                &suite_vars,
                env_file.as_deref(),
                &*ctx.os_env,
                &*ctx.backend,
                &ctx.allow,
                mode,
                ctx.budget,
            )
        };
        if mode == ResolveMode::ReferencesAsIs {
            return Ok(work());
        }
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|e| McpError::internal_error(format!("variable resolution failed: {e}"), None))
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
            McpError::invalid_params(format!("unknown resource uri: {}", request.uri), None)
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
            let resolved = self
                .resolve(
                    runtime_vars.clone(),
                    HashMap::new(),
                    env_file.clone(),
                    ResolveMode::Resolve,
                )
                .await?;
            let failed_names = failed_names(&resolved);
            let masker = Masker::new(resolved.secret_values());
            let mut unresolved: Vec<String> = Vec::new();
            let mut sub = |t: &str| {
                let r = substitute_with_check_failed(t, &resolved.vars, &failed_names);
                unresolved.extend(r.unresolved);
                r.text
            };
            let url = sub(&inline.url);
            let mut headers: HashMap<String, String> = inline
                .headers
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| {
                    let nv = sub(&v);
                    (k, nv)
                })
                .collect();
            let body = inline.body.as_deref().map(&mut sub);

            let blocked = blocking_failures(
                &resolved.failures,
                &resolved.vars,
                &unresolved,
                &headers,
                false,
            );
            if !blocked.is_empty() {
                let msg = blocked
                    .iter()
                    .map(|f| f.to_failure().actual)
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(McpError::internal_error(masker.mask_str(&msg), None));
            }

            inject_bearer(&mut headers, &resolved.vars, false);

            let response = execute_request(
                &client,
                &inline.method,
                &url,
                &headers,
                body.as_deref(),
                inline_timeout,
            )
            .await
            .map_err(|e| McpError::internal_error(masker.mask_str(&e.to_string()), None))?;

            let mut response = response;
            masker.mask_response(&mut response);
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

        let resolved = self
            .resolve(
                runtime_vars.clone(),
                suite.variables.clone(),
                env_file.clone(),
                ResolveMode::Resolve,
            )
            .await?;
        let failed_names = failed_names(&resolved);
        let masker = Masker::new(resolved.secret_values());

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
            let mut req_unresolved: Vec<String> = Vec::new();
            let url_result =
                substitute_with_check_failed(&req_def.url, &resolved.vars, &failed_names);
            req_unresolved.extend(url_result.unresolved.iter().cloned());
            all_unresolved.extend(url_result.unresolved);
            let url = url_result.text;

            let mut headers: HashMap<String, String> = req_def
                .headers
                .iter()
                .map(|(k, v)| {
                    let r = substitute_with_check_failed(v, &resolved.vars, &failed_names);
                    req_unresolved.extend(r.unresolved.iter().cloned());
                    all_unresolved.extend(r.unresolved);
                    (k.clone(), r.text)
                })
                .collect();

            let body = req_def.body.as_deref().map(|b| {
                let r = substitute_with_check_failed(b, &resolved.vars, &failed_names);
                req_unresolved.extend(r.unresolved.iter().cloned());
                all_unresolved.extend(r.unresolved);
                r.text
            });

            // シークレット参照の解決失敗: 該当変数を使うリクエストは送信せず失敗させる
            let blocked = blocking_failures(
                &resolved.failures,
                &resolved.vars,
                &req_unresolved,
                &headers,
                req_def.no_auth,
            );
            if !blocked.is_empty() {
                results.push(TestResult {
                    name: req_def.name.clone(),
                    status: 0,
                    passed: false,
                    elapsed_ms: 0,
                    failures: blocked.iter().map(|f| f.to_failure()).collect(),
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
                    error_type: Some("secret_resolution".to_string()),
                });
                if stop_on_failure {
                    break;
                }
                continue;
            }

            inject_bearer(&mut headers, &resolved.vars, req_def.no_auth);

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
        // Failed secret references are reported once, as SecretResolutionFailed.
        all_unresolved.retain(|n| !failed_names.contains(n));
        all_unresolved.sort();
        all_unresolved.dedup();
        if !all_unresolved.is_empty() {
            let var_list = all_unresolved.join(", ");
            warnings.push(format!(
                "Unresolved variables: {var_list} — use `config` tool to check variable resolution"
            ));
        }

        let mut suite_result = SuiteResult {
            total: results.len(),
            passed,
            failed,
            skipped,
            elapsed_ms: suite_elapsed,
            results,
            warnings,
        };

        // テンプレート/JSON/サマリに渡る前にマスクする
        masker.mask_suite(&mut suite_result);

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
            // parse never invokes secret CLIs: references are shown unresolved.
            let resolved = self
                .resolve(
                    HashMap::new(),
                    suite.variables.clone(),
                    env_file,
                    ResolveMode::ReferencesAsIs,
                )
                .await?;

            let mut output = format!("Variables ({}):\n", resolved.vars.len());
            let masker = Masker::new(resolved.secret_values());
            let secret_names: std::collections::HashSet<&str> = resolved
                .traces
                .iter()
                .filter(|t| t.secret)
                .map(|t| t.name.as_str())
                .collect();
            for (k, v) in &resolved.vars {
                let is_ref = resolved
                    .traces
                    .iter()
                    .any(|t| t.name == *k && t.reference.is_some());
                let shown = if secret_names.contains(k.as_str()) {
                    MASK.to_string()
                } else if is_ref {
                    format!("{v} (reference, not resolved)")
                } else {
                    masker.mask_str(v)
                };
                output.push_str(&format!("  {k} = {shown}\n"));
            }
            output.push_str(&format!("\nRequests ({}):\n", suite.requests.len()));
            for (i, r) in suite.requests.iter().enumerate() {
                let url = masker.mask_str(&substitute(&r.url, &resolved.vars));
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

        let resolved = self
            .resolve(
                runtime_vars,
                suite_vars.clone(),
                env_file,
                ResolveMode::Resolve,
            )
            .await?;

        // フィルタ適用
        let traces: Vec<VarTrace> = if let Some(filter) = &req.filter {
            let pattern = normalize_pattern(filter);
            resolved
                .traces
                .clone()
                .into_iter()
                .filter(|t| matches_simple_pattern(&t.name, &pattern))
                .collect()
        } else {
            resolved.traces.clone()
        };

        // Mise風出力
        let mut output = String::new();

        for trace in &traces {
            output.push_str(&format_trace_line(trace));
            output.push('\n');
        }
        for f in &resolved.failures {
            output.push_str(&format!("! {}\n", f.to_failure().actual));
        }

        // Bearer自動注入状態
        output.push('\n');
        match bearer_plan(&resolved.vars, &resolved.failures) {
            BearerPlan::Use(var) => {
                output.push_str(&format!("auto-bearer: ON (from ${var})\n"));
            }
            BearerPlan::Blocked(var) => output.push_str(&format!(
                "auto-bearer: BLOCKED ({var} secret resolution failed)\n"
            )),
            BearerPlan::Off => {
                output.push_str("auto-bearer: OFF (no $API_KEY or $BEARER_TOKEN found)\n");
            }
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
      body_matches:
        - 'beforeSystemDate=\d{4}-\d{2}-\d{2}'
      body_not_matches:
        - '"status":\s*"(failed|error)"'

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

## Secret References

A variable value may be `keychain://<service>/<account>` or `op://<vault>/<item>/<field>`.
It is resolved only if it matches a server-side `--secret-ref-allow <pattern>` flag
(repeatable; exact reference or `*` wildcard), whatever its source. Not allowed or
unresolvable => requests using it fail with `SecretResolutionFailed` (no default
fallback, the reference is never sent). Resolved values and `API_KEY` / `BEARER_TOKEN`
are masked (`***`) in all output, including percent-encoded and JSON-escaped forms
(base64 etc. are not masked). `parse` never resolves references.

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

/// 解決に失敗した変数名。
fn failed_names(resolved: &ResolvedVars) -> Vec<String> {
    resolved
        .failures
        .iter()
        .map(|f| f.name().to_string())
        .collect()
}

/// 1変数ぶんの config 出力行。シークレットの値は決して出力しない。
fn format_trace_line(trace: &VarTrace) -> String {
    let chain: Vec<String> = trace
        .found_in
        .iter()
        .map(|(src, val)| {
            let marker = if *src == trace.source {
                "here".to_string()
            } else if trace.secret && !is_reference(val) {
                MASK.to_string()
            } else {
                val.clone()
            };
            format!("{src}({marker})")
        })
        .collect();

    let display_value = if let Some(reference) = &trace.reference
        && trace.secret
    {
        format!(
            "{reference} -> <resolved, {} chars>",
            trace.value.chars().count()
        )
    } else if trace.secret {
        MASK.to_string()
    } else if trace.reference.is_some() {
        // 未解決の参照文字列はシークレットではないのでそのまま表示する
        format!("{} (reference, not resolved)", trace.value)
    } else if trace.value.chars().count() > 40 {
        let truncated: String = trace.value.chars().take(37).collect();
        format!("{truncated}...")
    } else {
        trace.value.clone()
    };

    format!(
        "${:<20} = {:<42} ({})",
        trace.name,
        display_value,
        chain.join(" -> ")
    )
}

/// Bearer 自動注入の判定（`inject_bearer` の変数選択と同一）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BearerPlan {
    /// 注入に使える変数が無い。
    Off,
    /// この変数の値で注入される。
    Use(&'static str),
    /// 選ばれる変数のシークレット解決が失敗している（別変数へ黙って
    /// フォールバックしない）。
    Blocked(&'static str),
}

/// `inject_bearer` と同じ順序（API_KEY → BEARER_TOKEN）で変数を選ぶ。
/// API_KEY が失敗なら BEARER_TOKEN へフォールバックせずブロックする。
fn bearer_plan(vars: &HashMap<String, String>, failures: &[ResolveFailure]) -> BearerPlan {
    for var in ["API_KEY", "BEARER_TOKEN"] {
        if failures.iter().any(|f| f.name() == var) {
            return BearerPlan::Blocked(var);
        }
        if vars.contains_key(var) {
            return BearerPlan::Use(var);
        }
    }
    BearerPlan::Off
}

/// このリクエストの実行を妨げるシークレット解決失敗を返す。
///
/// - リクエスト内で参照された（未解決になった）変数が失敗した変数である場合
/// - Bearer 自動注入が有効で、[`bearer_plan`] が選ぶ変数の解決が失敗している場合
fn blocking_failures<'a>(
    failures: &'a [ResolveFailure],
    vars: &HashMap<String, String>,
    unresolved: &[String],
    headers: &HashMap<String, String>,
    no_auth: bool,
) -> Vec<&'a ResolveFailure> {
    let bearer_applies = !no_auth
        && !headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("authorization"));
    let blocked_bearer = match bearer_plan(vars, failures) {
        BearerPlan::Blocked(v) if bearer_applies => Some(v),
        _ => None,
    };
    failures
        .iter()
        .filter(|f| unresolved.iter().any(|u| u == f.name()) || blocked_bearer == Some(f.name()))
        .collect()
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

    // ---- secret references -------------------------------------------------

    use crate::secret::SecretBackend;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Fake backend that records every call.
    struct Rec {
        reply: Result<String, String>,
        calls: Mutex<Vec<String>>,
    }
    impl Rec {
        fn ok(v: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: Ok(v.to_string()),
                calls: Default::default(),
            })
        }
        fn err() -> Arc<Self> {
            Arc::new(Self {
                reply: Err("backend failed".to_string()),
                calls: Default::default(),
            })
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }
    impl SecretBackend for Rec {
        fn resolve(&self, reference: &str) -> Result<String, String> {
            self.calls.lock().unwrap().push(reference.to_string());
            self.reply.clone()
        }
    }

    fn server_with(allow: &[&str], backend: Arc<Rec>) -> ApiVerifierServer {
        ApiVerifierServer::new(SecretCtx {
            allow: Arc::new(SecretAllowlist::new(allow.iter().map(|s| s.to_string()))),
            backend,
            // hermetic: the host OS environment is never consulted
            os_env: Arc::new(|_| None),
            budget: crate::secret::RESOLVE_DEADLINE,
        })
    }

    fn tmp_file(content: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    fn result_text(res: &CallToolResult) -> String {
        let v = serde_json::to_value(res).unwrap();
        v["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn no_os(_: &str) -> Option<String> {
        None
    }

    fn resolve_plain(
        runtime: &HashMap<String, String>,
        suite: &HashMap<String, String>,
        backend: &dyn SecretBackend,
        allow: &[&str],
        mode: ResolveMode,
        env_file: Option<&std::path::Path>,
    ) -> ResolvedVars {
        resolve_variables_in(
            runtime,
            suite,
            env_file,
            &no_os,
            backend,
            &SecretAllowlist::new(allow.iter().map(|s| s.to_string())),
            mode,
            crate::secret::RESOLVE_DEADLINE,
        )
    }

    #[test]
    fn config_line_shows_reference_and_length_never_value() {
        let secret = "resolved-secret-value-42";
        let f = tmp_file("API_KEY=keychain://svc/acct\n");
        let r = resolve_plain(
            &HashMap::new(),
            &HashMap::new(),
            &*Rec::ok(secret),
            &["keychain://svc/*"],
            ResolveMode::Resolve,
            Some(f.path()),
        );
        let line = format_trace_line(&r.traces[0]);
        assert!(
            line.contains("keychain://svc/acct -> <resolved, 24 chars>"),
            "{line}"
        );
        assert!(!line.contains(secret));
    }

    #[test]
    fn config_line_shows_unresolved_reference_for_bearer_names() {
        // finding 8: the reference string is not a secret, show it
        let runtime = HashMap::from([("API_KEY".to_string(), "op://v/i/f".to_string())]);
        let r = resolve_plain(
            &runtime,
            &HashMap::new(),
            &*Rec::ok("never"),
            &["*"],
            ResolveMode::ReferencesAsIs,
            None,
        );
        assert!(!r.traces[0].secret);
        assert!(r.secret_values().is_empty());
        let line = format_trace_line(&r.traces[0]);
        assert!(
            line.contains("op://v/i/f (reference, not resolved)"),
            "{line}"
        );
        assert!(!line.contains(MASK), "{line}");
    }

    #[test]
    fn config_line_masks_literal_bearer_value_and_chain() {
        let r = resolve_plain(
            &HashMap::from([("API_KEY".to_string(), "runtime-literal-key".to_string())]),
            &HashMap::from([("API_KEY".to_string(), "suite-literal-key-1".to_string())]),
            &*Rec::err(),
            &[],
            ResolveMode::Resolve,
            None,
        );
        let line = format_trace_line(&r.traces[0]);
        assert!(!line.contains("literal-key"), "{line}");
        assert!(line.contains(MASK));
    }

    #[test]
    fn config_line_plain_var_unchanged() {
        let r = resolve_plain(
            &HashMap::from([("BASE_URL".to_string(), "http://x".to_string())]),
            &HashMap::new(),
            &*Rec::err(),
            &[],
            ResolveMode::Resolve,
            None,
        );
        let line = format_trace_line(&r.traces[0]);
        assert!(line.contains("http://x"));
        assert!(line.contains("runtime(here)"));
    }

    #[test]
    fn failure_blocks_requests_using_variable_or_bearer() {
        let f = tmp_file("API_KEY=op://v/i/f\nOTHER=op://v/i/g\n");
        let r = resolve_plain(
            &HashMap::new(),
            &HashMap::new(),
            &*Rec::err(),
            &["op://*"],
            ResolveMode::Resolve,
            Some(f.path()),
        );
        assert_eq!(r.failures.len(), 2);
        let none: HashMap<String, String> = HashMap::new();
        let b = blocking_failures(&r.failures, &r.vars, &[], &none, false);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].name(), "API_KEY");
        assert!(blocking_failures(&r.failures, &r.vars, &[], &none, true).is_empty());
        let b = blocking_failures(&r.failures, &r.vars, &["OTHER".to_string()], &none, true);
        assert_eq!(b[0].name(), "OTHER");
        let auth = HashMap::from([("authorization".to_string(), "x".to_string())]);
        assert!(blocking_failures(&r.failures, &r.vars, &[], &auth, false).is_empty());
    }

    fn fail(name: &str) -> ResolveFailure {
        ResolveFailure::SecretResolutionFailed {
            name: name.to_string(),
            reference: "keychain://svc/x".to_string(),
            reason: "boom".to_string(),
        }
    }

    #[test]
    fn auto_bearer_selection_mirrors_inject_bearer() {
        let none = HashMap::new();
        let vars_bt = HashMap::from([("BEARER_TOKEN".to_string(), "bt-value".to_string())]);
        let vars_both = HashMap::from([
            ("API_KEY".to_string(), "ak-value".to_string()),
            ("BEARER_TOKEN".to_string(), "bt-value".to_string()),
        ]);
        // (1) API_KEY failed -> block, no silent fallback to BEARER_TOKEN
        let f = [fail("API_KEY")];
        assert_eq!(bearer_plan(&vars_bt, &f), BearerPlan::Blocked("API_KEY"));
        let b = blocking_failures(&f, &vars_bt, &[], &none, false);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].name(), "API_KEY");
        // (2) API_KEY resolved -> a BEARER_TOKEN failure is irrelevant
        let f = [fail("BEARER_TOKEN")];
        assert_eq!(bearer_plan(&vars_both, &f), BearerPlan::Use("API_KEY"));
        assert!(blocking_failures(&f, &vars_both, &[], &none, false).is_empty());
        // (3) API_KEY absent and BEARER_TOKEN failed -> block
        assert_eq!(bearer_plan(&none, &f), BearerPlan::Blocked("BEARER_TOKEN"));
        let b = blocking_failures(&f, &none, &[], &none, false);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].name(), "BEARER_TOKEN");
        // nothing defined
        assert_eq!(bearer_plan(&none, &[]), BearerPlan::Off);
    }

    #[tokio::test]
    async fn config_reports_blocked_auto_bearer_consistently_with_run() {
        let env = tmp_file("API_KEY=keychain://evil/x\nBEARER_TOKEN=plain-bearer-token\n");
        let srv = server_with(&["keychain://svc/*"], Rec::ok("unused-secret-val"));
        let res = srv
            .config(Parameters(McpConfigRequest {
                env_file: env.path().to_str().map(|s| s.to_string()),
                http_file: None,
                variables: None,
                filter: None,
            }))
            .await
            .unwrap();
        let text = result_text(&res);
        assert!(
            text.contains("auto-bearer: BLOCKED (API_KEY secret resolution failed)"),
            "{text}"
        );
        assert!(!text.contains("auto-bearer: ON"), "{text}");
    }

    struct SlowRec {
        calls: Mutex<Vec<String>>,
    }
    impl SecretBackend for SlowRec {
        fn resolve(&self, reference: &str) -> Result<String, String> {
            self.calls.lock().unwrap().push(reference.to_string());
            std::thread::sleep(std::time::Duration::from_millis(80));
            Ok("slow-secret-value-1".to_string())
        }
    }

    #[tokio::test]
    async fn overall_resolution_deadline_fails_remaining_references() {
        let env = tmp_file("A=keychain://svc/a\nB=keychain://svc/b\nC=keychain://svc/c\n");
        let backend = Arc::new(SlowRec {
            calls: Default::default(),
        });
        let srv = ApiVerifierServer::new(SecretCtx {
            allow: Arc::new(SecretAllowlist::new(["keychain://svc/*".to_string()])),
            backend: backend.clone(),
            os_env: Arc::new(|_| None),
            budget: std::time::Duration::from_millis(50),
        });
        let res = srv
            .config(Parameters(McpConfigRequest {
                env_file: env.path().to_str().map(|s| s.to_string()),
                http_file: None,
                variables: None,
                filter: None,
            }))
            .await
            .unwrap();
        let text = result_text(&res);
        assert_eq!(backend.calls.lock().unwrap().len(), 1, "{text}");
        assert_eq!(
            text.matches("overall secret resolution deadline exceeded")
                .count(),
            2,
            "{text}"
        );
    }

    #[tokio::test]
    async fn failed_variable_is_reported_once_not_as_unresolved_warning() {
        let (base, _hits) = spawn_echo().await;
        let yaml = format!(
            "variables:\n  BASE: \"{base}\"\nrequests:\n  - name: r\n    method: GET\n    url: \"{{{{BASE}}}}/p?k={{{{TOK}}}}&u={{{{NOPE}}}}\"\n    expect:\n      status: 200\n"
        );
        let suite = tmp_file(&yaml);
        let env = tmp_file("TOK=keychain://svc/acct\n");
        let srv = server_with(&["keychain://svc/*"], Rec::err());
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("o.json");
        srv.run(Parameters(run_req(
            suite.path().to_str(),
            env.path().to_str().unwrap(),
            out.to_str().unwrap(),
            "json",
        )))
        .await
        .unwrap();
        let file = std::fs::read_to_string(&out).unwrap();
        assert!(file.contains("SecretResolutionFailed"), "{file}");
        // genuinely undefined NOPE is still warned about, TOK is not
        assert!(file.contains("Unresolved variables: NOPE "), "{file}");
    }

    /// Minimal echo server: replies with the raw request head as the body.
    /// Returns its base URL and a counter of requests received.
    async fn spawn_echo() -> (String, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    counter.fetch_add(1, Ordering::SeqCst);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        buf.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.write_all(&buf).await;
                });
            }
        });
        (format!("http://{addr}"), hits)
    }

    fn run_req(file: Option<&str>, env_file: &str, out: &str, format: &str) -> McpRunRequest {
        McpRunRequest {
            file: file.map(|s| s.to_string()),
            request: None,
            filter: None,
            env_file: Some(env_file.to_string()),
            variables: None,
            stop_on_failure: None,
            delay_ms: None,
            timeout_ms: Some(5000),
            format: Some(format.to_string()),
            template: None,
            body_max_lines: None,
            output_file: Some(out.to_string()),
            no_output_file: None,
        }
    }

    fn suite_yaml(base: &str, url_tail: &str) -> String {
        format!(
            "variables:\n  BASE: \"{base}\"\nrequests:\n  - name: echo\n    method: GET\n    url: \"{{{{BASE}}}}/{url_tail}\"\n    headers:\n      X-Tok: \"{{{{TOK}}}}\"\n    expect:\n      status: 200\n      body_contains: [\"never-in-body\"]\n"
        )
    }

    #[tokio::test]
    async fn run_handler_masks_summary_json_and_markdown() {
        let secret = "handler-secret/token+1";
        let (base, hits) = spawn_echo().await;
        let suite = tmp_file(&suite_yaml(&base, "p?k={{TOK}}"));
        let env = tmp_file("TOK=keychain://svc/acct\n");
        let backend = Rec::ok(secret);
        let srv = server_with(&["keychain://svc/*"], backend.clone());
        let dir = tempfile::tempdir().unwrap();
        for fmt in ["summary", "json", "markdown"] {
            let out = dir.path().join(format!("out-{fmt}.txt"));
            let req = run_req(
                suite.path().to_str(),
                env.path().to_str().unwrap(),
                out.to_str().unwrap(),
                fmt,
            );
            let res = srv.run(Parameters(req)).await.unwrap();
            let text = result_text(&res);
            let file = std::fs::read_to_string(&out).unwrap();
            // raw, percent-encoded and json-escaped forms are all absent
            for forbidden in [
                secret,
                "handler-secret%2Ftoken%2B1",
                "handler-secret\\/token+1",
            ] {
                assert!(!text.contains(forbidden), "{fmt} summary: {text}");
                assert!(!file.contains(forbidden), "{fmt} file: {file}");
            }
            assert!(file.contains("***"), "{fmt}: {file}");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 3, "requests really went out");
        assert!(backend.calls().iter().all(|c| c == "keychain://svc/acct"));
    }

    #[tokio::test]
    async fn run_handler_rejects_agent_supplied_env_file_reference() {
        let (base, hits) = spawn_echo().await;
        let suite = tmp_file(&suite_yaml(&base, "p?k={{TOK}}"));
        // the agent controls env_file: this reference is not on the allowlist
        let env = tmp_file("TOK=keychain://evil/acct\n");
        let backend = Rec::ok("must-never-be-fetched");
        let srv = server_with(&["keychain://my-api-tests/*"], backend.clone());
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.json");
        let req = run_req(
            suite.path().to_str(),
            env.path().to_str().unwrap(),
            out.to_str().unwrap(),
            "json",
        );
        let res = srv.run(Parameters(req)).await.unwrap();
        assert!(result_text(&res).contains("1 failed"));
        assert!(backend.calls().is_empty(), "backend must not be called");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "request must not be sent");
        let file = std::fs::read_to_string(&out).unwrap();
        assert!(file.contains("SecretResolutionFailed"), "{file}");
        assert!(file.contains("not allowed by --secret-ref-allow"), "{file}");
        assert!(file.contains("secret_resolution"), "{file}");
        assert!(!file.contains("must-never-be-fetched"));
    }

    #[tokio::test]
    async fn run_handler_without_any_allow_flag_rejects_references() {
        let (base, hits) = spawn_echo().await;
        let suite = tmp_file(&suite_yaml(&base, "p"));
        let env = tmp_file("TOK=op://Dev/item/field\n");
        let backend = Rec::ok("must-never-be-fetched");
        let srv = server_with(&[], backend.clone());
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("o.json");
        let res = srv
            .run(Parameters(run_req(
                suite.path().to_str(),
                env.path().to_str().unwrap(),
                out.to_str().unwrap(),
                "json",
            )))
            .await
            .unwrap();
        assert!(result_text(&res).contains("1 failed"));
        assert!(backend.calls().is_empty());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn default_syntax_does_not_mask_a_failed_secret() {
        let (base, hits) = spawn_echo().await;
        let yaml = format!(
            "variables:\n  BASE: \"{base}\"\nrequests:\n  - name: dflt\n    method: GET\n    url: \"{{{{BASE}}}}/p?k=${{TOK:fallback}}\"\n    expect:\n      status: 200\n"
        );
        let suite = tmp_file(&yaml);
        let env = tmp_file("TOK=keychain://svc/acct\n");
        let srv = server_with(&["keychain://svc/*"], Rec::err());
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("o.json");
        let res = srv
            .run(Parameters(run_req(
                suite.path().to_str(),
                env.path().to_str().unwrap(),
                out.to_str().unwrap(),
                "json",
            )))
            .await
            .unwrap();
        assert!(result_text(&res).contains("1 failed"));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "must be blocked, not defaulted"
        );
        let file = std::fs::read_to_string(&out).unwrap();
        assert!(file.contains("SecretResolutionFailed"), "{file}");
    }

    fn inline_req(url: &str, vars: &[(&str, &str)]) -> McpRunRequest {
        McpRunRequest {
            file: None,
            request: Some(InlineRequest {
                method: "GET".into(),
                url: url.into(),
                headers: Some(HashMap::from([(
                    "X-Tok".to_string(),
                    "{{TOK}}".to_string(),
                )])),
                body: None,
            }),
            filter: None,
            env_file: None,
            variables: Some(
                vars.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            stop_on_failure: None,
            delay_ms: None,
            timeout_ms: Some(5000),
            format: None,
            template: None,
            body_max_lines: None,
            output_file: None,
            no_output_file: Some(true),
        }
    }

    #[tokio::test]
    async fn inline_request_with_allowlisted_runtime_reference_is_masked() {
        let secret = "inline-secret-token-9";
        let (base, hits) = spawn_echo().await;
        let backend = Rec::ok(secret);
        let srv = server_with(&["keychain://svc/*"], backend.clone());
        let res = srv
            .run(Parameters(inline_req(
                &format!("{base}/p?k={{{{TOK}}}}"),
                &[("TOK", "keychain://svc/rt")],
            )))
            .await
            .unwrap();
        let text = result_text(&res);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(backend.calls(), vec!["keychain://svc/rt".to_string()]);
        assert!(!text.contains(secret), "{text}");
        assert!(text.contains("***"), "{text}");
    }

    #[tokio::test]
    async fn inline_request_rejects_non_allowlisted_runtime_reference() {
        let (base, hits) = spawn_echo().await;
        let backend = Rec::ok("never");
        let srv = server_with(&["keychain://svc/*"], backend.clone());
        let err = srv
            .run(Parameters(inline_req(
                &format!("{base}/p?k={{{{TOK}}}}"),
                &[("TOK", "keychain://other/x")],
            )))
            .await
            .unwrap_err();
        assert!(err.message.contains("not allowed by --secret-ref-allow"));
        assert!(backend.calls().is_empty());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn inline_request_transport_error_is_masked() {
        let secret = "inline-err-secret-77";
        let srv = server_with(&["keychain://svc/*"], Rec::ok(secret));
        // port 1: connection refused; the URL (with the secret) must not leak
        let err = srv
            .run(Parameters(inline_req(
                "http://127.0.0.1:1/p?k={{TOK}}",
                &[("TOK", "keychain://svc/rt")],
            )))
            .await
            .unwrap_err();
        assert!(!err.message.contains(secret), "{}", err.message);
    }

    #[tokio::test]
    async fn config_handler_hides_value_and_reports_disallowed() {
        let secret = "config-secret-value-5";
        let env = tmp_file("API_KEY=keychain://svc/acct\nBAD=keychain://evil/x\nPLAIN=hi\n");
        let backend = Rec::ok(secret);
        let srv = server_with(&["keychain://svc/*"], backend.clone());
        let res = srv
            .config(Parameters(McpConfigRequest {
                env_file: env.path().to_str().map(|s| s.to_string()),
                http_file: None,
                variables: None,
                filter: None,
            }))
            .await
            .unwrap();
        let text = result_text(&res);
        assert!(!text.contains(secret), "{text}");
        assert!(
            text.contains("keychain://svc/acct -> <resolved, 21 chars>"),
            "{text}"
        );
        assert!(text.contains("not allowed by --secret-ref-allow"), "{text}");
        assert!(text.contains("PLAIN"), "{text}");
        assert_eq!(backend.calls(), vec!["keychain://svc/acct".to_string()]);
    }

    #[tokio::test]
    async fn parse_handler_never_invokes_backend_and_shows_references() {
        let suite = tmp_file(
            "variables:\n  API_KEY: \"keychain://svc/acct\"\n  BEARER_TOKEN: literal-bearer-value\n  BASE: http://h\nrequests:\n  - name: r\n    method: GET\n    url: \"{{BASE}}/x?t={{BEARER_TOKEN}}\"\n",
        );
        let backend = Rec::ok("must-never-be-fetched");
        let srv = server_with(&["*"], backend.clone());
        let res = srv
            .parse(Parameters(McpParseRequest {
                path: suite.path().to_str().unwrap().to_string(),
                resolve_vars: Some(true),
                env_file: None,
            }))
            .await
            .unwrap();
        let text = result_text(&res);
        assert!(backend.calls().is_empty(), "parse must not call backends");
        assert!(
            text.contains("API_KEY = keychain://svc/acct (reference, not resolved)"),
            "{text}"
        );
        assert!(!text.contains("literal-bearer-value"), "{text}");
        assert!(!text.contains("must-never-be-fetched"));
    }

    #[tokio::test]
    async fn resolved_api_key_reaches_the_wire_and_is_masked_in_output() {
        let secret = "wire-secret-token-777";
        let (base, _) = spawn_echo().await;
        let f = tmp_file("API_KEY=keychain://svc/acct\n");
        let resolved = resolve_plain(
            &HashMap::new(),
            &HashMap::new(),
            &*Rec::ok(secret),
            &["keychain://svc/*"],
            ResolveMode::Resolve,
            Some(f.path()),
        );
        let mut headers = HashMap::new();
        assert_eq!(
            inject_bearer(&mut headers, &resolved.vars, false),
            Some("API_KEY".to_string())
        );
        let client = build_client().unwrap();
        let mut resp = execute_request(&client, "GET", &format!("{base}/"), &headers, None, 5000)
            .await
            .unwrap();
        assert!(
            resp.body
                .to_lowercase()
                .contains(&format!("authorization: bearer {secret}").to_lowercase()),
            "{}",
            resp.body
        );
        let masker = Masker::new(resolved.secret_values());
        masker.mask_response(&mut resp);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains(secret), "{json}");
    }
}
