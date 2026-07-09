use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// =============================================================================
// Test suite definition (YAML input)
// =============================================================================

/// テストスイート定義。YAMLファイルの最上位構造。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TestSuite {
    /// ファイルレベルの変数定義。
    #[serde(default)]
    pub variables: HashMap<String, String>,

    /// スイート全体のデフォルトタイムアウト (ms)。省略時は run の timeout_ms を使用。
    #[serde(default)]
    pub timeout_ms: Option<u64>,

    /// リクエスト一覧。
    pub requests: Vec<RequestDef>,
}

/// 1リクエストの定義。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RequestDef {
    /// テスト名。
    pub name: String,

    /// HTTPメソッド (GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS)。
    pub method: String,

    /// リクエストURL。変数展開可 (`{{VAR}}`, `$VAR`, `${VAR}`)。
    pub url: String,

    /// リクエストヘッダー。
    #[serde(default)]
    pub headers: HashMap<String, String>,

    /// リクエストボディ。JSONやフォームデータ等。
    #[serde(default)]
    pub body: Option<String>,

    /// 期待値。省略時はステータスコードのみ確認しない。
    #[serde(default)]
    pub expect: Option<ExpectDef>,

    /// Bearer自動注入を無効化。
    #[serde(default)]
    pub no_auth: bool,

    /// このリクエストのタイムアウト (ms)。省略時はスイートレベルの値を使用。
    #[serde(default)]
    pub timeout_ms: Option<u64>,

    /// このテストをスキップ。
    #[serde(default)]
    pub skip: bool,

    /// タグ（フィルタで使用: `@tag:smoke`）。
    #[serde(default)]
    pub tags: Vec<String>,
}

/// 期待値定義。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExpectDef {
    /// 期待HTTPステータスコード。単一 or 複数。
    #[serde(default)]
    pub status: Option<StatusExpect>,

    /// ヘッダー検証。キーはcase-insensitive、値は部分一致。
    #[serde(default)]
    pub headers: HashMap<String, String>,

    /// ボディに含まれるべき文字列。
    #[serde(default)]
    pub body_contains: Vec<String>,

    /// ボディに含まれてはいけない文字列。
    #[serde(default)]
    pub body_not_contains: Vec<String>,

    /// ボディがマッチすべき正規表現（Rust regex構文）。ボディ全体に対して検索 (is_match) する。
    #[serde(default)]
    pub body_matches: Vec<String>,

    /// ボディがマッチしてはいけない正規表現（Rust regex構文）。ボディ全体に対して検索 (is_match) する。
    #[serde(default)]
    pub body_not_matches: Vec<String>,
}

/// 期待ステータスコード。単一値 or 複数値。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum StatusExpect {
    Single(u16),
    Multiple(Vec<u16>),
}

impl StatusExpect {
    pub fn matches(&self, actual: u16) -> bool {
        match self {
            Self::Single(s) => *s == actual,
            Self::Multiple(v) => v.contains(&actual),
        }
    }
}

impl std::fmt::Display for StatusExpect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Single(s) => write!(f, "{s}"),
            Self::Multiple(v) => {
                let s: Vec<String> = v.iter().map(|n| n.to_string()).collect();
                write!(f, "{}", s.join(" or "))
            }
        }
    }
}

// =============================================================================
// Variable resolution types
// =============================================================================

/// 変数の出所。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VarSource {
    /// run の variables パラメータ。
    Runtime,
    /// YAMLファイル内の variables。
    SuiteFile,
    /// .env ファイル。
    DotEnv,
    /// OS環境変数。
    OsEnv,
}

impl std::fmt::Display for VarSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime => write!(f, "runtime"),
            Self::SuiteFile => write!(f, "suite"),
            Self::DotEnv => write!(f, ".env"),
            Self::OsEnv => write!(f, "OS"),
        }
    }
}

/// 変数解決の追跡情報。config ツールの出力に使う。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VarTrace {
    pub name: String,
    pub value: String,
    pub source: VarSource,
    /// この変数が見つかった全ソースと値。低い優先順位から高い順。
    pub found_in: Vec<(VarSource, String)>,
}

/// 解決済み変数セット。
#[derive(Debug, Clone, Default)]
pub struct ResolvedVars {
    pub vars: HashMap<String, String>,
    pub traces: Vec<VarTrace>,
}

// =============================================================================
// HTTP execution types
// =============================================================================

/// HTTPリクエスト実行結果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub http_version: String,
    pub headers: HashMap<String, String>,
    /// reqwest が実際に送信したリクエストヘッダー。
    pub actual_request_headers: HashMap<String, String>,
    pub body: String,
    pub charset: Option<String>,
    pub elapsed_ms: u64,
    pub size_bytes: usize,
}

// =============================================================================
// Assertion types
// =============================================================================

/// 1つの検証失敗。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    pub check: String,
    pub expected: String,
    pub actual: String,
}

/// 1リクエストの検証結果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub name: String,
    pub status: u16,
    pub passed: bool,
    pub elapsed_ms: u64,
    pub failures: Vec<Failure>,
    pub response_preview: String,
    pub request_method: String,
    pub request_url: String,
    pub request_headers: HashMap<String, String>,
    pub request_body: Option<String>,
    pub response_headers: HashMap<String, String>,
    /// HTTPバージョン（"HTTP/1.1", "HTTP/2" 等）。
    #[serde(default)]
    pub response_http_version: String,
    /// レスポンスの文字コード（content-type から検出）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_charset: Option<String>,
    /// レスポンスボディのバイト数。
    #[serde(default)]
    pub response_size_bytes: usize,
    pub skipped: bool,
    /// エラー種別（"timeout", "connection", "dns", "tls", None=成功 or アサート失敗）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
}

/// スイート全体の結果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteResult {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub elapsed_ms: u64,
    pub results: Vec<TestResult>,
    /// 警告メッセージ（未解決変数、circuit breaker 等）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}
