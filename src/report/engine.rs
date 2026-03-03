use std::collections::HashMap;

use minijinja::{Environment, Value};

use crate::types::SuiteResult;

const DEFAULT_TEMPLATE: &str = include_str!("default.md.j2");

/// スイート結果をテンプレートでレンダリングする。
/// `body_max_lines`: None = 全文出力、Some(n) = n行に切り詰め。
pub fn render(
    result: &SuiteResult,
    custom_template: Option<&str>,
    body_max_lines: Option<usize>,
) -> Result<String, String> {
    let mut env = Environment::new();
    env.add_function("truncate_lines", truncate_lines);

    let template_source = custom_template.unwrap_or(DEFAULT_TEMPLATE);
    env.add_template("report", template_source)
        .map_err(|e| format!("Template parse error: {e}"))?;

    let tmpl = env
        .get_template("report")
        .map_err(|e| format!("Template load error: {e}"))?;

    // エラーグルーピング: 同一エラーメッセージをカウント
    let error_groups: Vec<HashMap<&str, Value>> = {
        let mut groups: Vec<(String, usize)> = Vec::new();
        for r in &result.results {
            if r.passed || r.skipped {
                continue;
            }
            if let Some(f) = r.failures.first() {
                if let Some(entry) = groups.iter_mut().find(|(msg, _)| *msg == f.actual) {
                    entry.1 += 1;
                } else {
                    groups.push((f.actual.clone(), 1));
                }
            }
        }
        groups
            .into_iter()
            .map(|(msg, count)| {
                let mut m = HashMap::new();
                m.insert("message", Value::from(msg));
                m.insert("count", Value::from(count));
                m
            })
            .collect()
    };

    let ctx = minijinja::context! {
        total => result.total,
        passed => result.passed,
        failed => result.failed,
        skipped => result.skipped,
        elapsed_ms => result.elapsed_ms,
        results => Value::from_serialize(&result.results),
        warnings => Value::from_serialize(&result.warnings),
        error_groups => Value::from_serialize(&error_groups),
        body_max_lines => body_max_lines.map(Value::from).unwrap_or(Value::UNDEFINED),
    };

    tmpl.render(ctx)
        .map_err(|e| format!("Template render error: {e}"))
}

/// テンプレートフィルタ: 先頭N行のみ表示。
fn truncate_lines(text: Value, max_lines: Value) -> Result<String, minijinja::Error> {
    let text = text.as_str().unwrap_or_default();
    let max = max_lines.as_usize().unwrap_or(6);

    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return Ok(text.to_string());
    }

    let truncated: Vec<&str> = lines[..max].to_vec();
    Ok(format!(
        "{}\n... ({}行中 先頭{}行のみ表示)",
        truncated.join("\n"),
        lines.len(),
        max
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TestResult;

    fn make_suite(results: Vec<TestResult>) -> SuiteResult {
        let total = results.len();
        let passed = results.iter().filter(|r| r.passed).count();
        let failed = results.iter().filter(|r| !r.passed && !r.skipped).count();
        let skipped = results.iter().filter(|r| r.skipped).count();
        SuiteResult {
            total,
            passed,
            failed,
            skipped,
            elapsed_ms: 1000,
            results,
            warnings: Vec::new(),
        }
    }

    fn make_test_result(name: &str, passed: bool, status: u16) -> TestResult {
        TestResult {
            name: name.to_string(),
            status,
            passed,
            elapsed_ms: 100,
            failures: Vec::new(),
            response_preview: String::new(),
            request_method: "GET".to_string(),
            request_url: "https://example.com".to_string(),
            request_headers: std::collections::HashMap::new(),
            request_body: None,
            response_headers: std::collections::HashMap::new(),
            response_http_version: String::new(),
            response_charset: None,
            response_size_bytes: 0,
            skipped: false,
            error_type: None,
        }
    }

    #[test]
    fn render_default_template() {
        let suite = make_suite(vec![
            make_test_result("テスト1", true, 200),
            make_test_result("テスト2", false, 500),
        ]);
        let output = render(&suite, None, None).unwrap();
        assert!(output.contains("2 tests"));
        assert!(output.contains("1 passed"));
        assert!(output.contains("1 failed"));
    }
}
