use std::path::Path;

use crate::types::TestSuite;

/// YAMLファイルからテストスイートを読み込む。
pub fn load_suite(path: &Path) -> Result<TestSuite, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    load_suite_str(&content)
}

/// YAML文字列からテストスイートをパースする。
pub fn load_suite_str(content: &str) -> Result<TestSuite, String> {
    serde_yml::from_str(content).map_err(|e| format!("YAML parse error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_minimal() {
        let yaml = r#"
requests:
  - name: ヘルスチェック
    method: GET
    url: https://example.com/health
    expect:
      status: 200
"#;
        let suite = load_suite_str(yaml).unwrap();
        assert_eq!(suite.requests.len(), 1);
        assert_eq!(suite.requests[0].name, "ヘルスチェック");
        assert_eq!(suite.requests[0].method, "GET");
    }

    #[test]
    fn load_with_variables() {
        let yaml = r#"
variables:
  BASE_URL: https://example.com/api
  ASSET_ID: abc123

requests:
  - name: テスト
    method: GET
    url: "{{BASE_URL}}/health"
"#;
        let suite = load_suite_str(yaml).unwrap();
        assert_eq!(
            suite.variables.get("BASE_URL").unwrap(),
            "https://example.com/api"
        );
        assert_eq!(suite.requests.len(), 1);
    }

    #[test]
    fn load_full_request() {
        let yaml = r#"
requests:
  - name: エクスポート
    method: POST
    url: https://example.com/api/export
    headers:
      Content-Type: application/json
    body: '{"assetId": "abc123"}'
    timeout_ms: 60000
    tags: [csv, export]
    expect:
      status: 200
      headers:
        content-type: text/csv
        x-result-count: "15"
      body_contains:
        - 会社名
        - メールアドレス
      body_not_contains:
        - error
"#;
        let suite = load_suite_str(yaml).unwrap();
        let req = &suite.requests[0];
        assert_eq!(req.method, "POST");
        assert_eq!(req.timeout_ms, Some(60000));
        assert_eq!(req.tags, vec!["csv", "export"]);

        let expect = req.expect.as_ref().unwrap();
        assert_eq!(expect.headers.get("content-type").unwrap(), "text/csv");
        assert_eq!(expect.body_contains, vec!["会社名", "メールアドレス"]);
        assert_eq!(expect.body_not_contains, vec!["error"]);
    }

    #[test]
    fn load_multiple_status() {
        let yaml = r#"
requests:
  - name: テスト
    method: GET
    url: https://example.com
    expect:
      status: [200, 404]
"#;
        let suite = load_suite_str(yaml).unwrap();
        let expect = suite.requests[0].expect.as_ref().unwrap();
        let status = expect.status.as_ref().unwrap();
        assert!(status.matches(200));
        assert!(status.matches(404));
        assert!(!status.matches(500));
    }

    #[test]
    fn load_skip_and_no_auth() {
        let yaml = r#"
requests:
  - name: スキップ
    method: GET
    url: https://example.com
    skip: true
    no_auth: true
"#;
        let suite = load_suite_str(yaml).unwrap();
        assert!(suite.requests[0].skip);
        assert!(suite.requests[0].no_auth);
    }

    #[test]
    fn invalid_yaml_returns_error() {
        let yaml = "{{invalid: yaml";
        assert!(load_suite_str(yaml).is_err());
    }

    #[test]
    fn load_with_suite_timeout() {
        let yaml = r#"
timeout_ms: 60000

requests:
  - name: dummy
    method: GET
    url: https://example.com/dummy
"#;
        let suite = load_suite_str(yaml).unwrap();
        assert_eq!(suite.timeout_ms, Some(60000));
    }

    #[test]
    fn load_without_suite_timeout() {
        let yaml = r#"
requests:
  - name: dummy
    method: GET
    url: https://example.com/dummy
"#;
        let suite = load_suite_str(yaml).unwrap();
        assert_eq!(suite.timeout_ms, None);
    }
}
