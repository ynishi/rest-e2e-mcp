use regex::Regex;

use crate::types::{ExpectDef, Failure, HttpResponse};

/// レスポンスを期待値と照合し、失敗リストを返す。
pub fn check(expect: Option<&ExpectDef>, response: &HttpResponse) -> Vec<Failure> {
    let Some(expect) = expect else {
        return Vec::new();
    };

    let mut failures = Vec::new();

    // ステータスコード検証
    if let Some(status) = &expect.status
        && !status.matches(response.status)
    {
        failures.push(Failure {
            check: "status".to_string(),
            expected: status.to_string(),
            actual: response.status.to_string(),
        });
    }

    // ヘッダー検証（キーはcase-insensitive、値は部分一致）
    for (key, expected_val) in &expect.headers {
        let actual_val = response
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str());

        match actual_val {
            None => {
                failures.push(Failure {
                    check: format!("header[{key}]"),
                    expected: expected_val.clone(),
                    actual: "(missing)".to_string(),
                });
            }
            Some(actual) if !actual.contains(expected_val.as_str()) => {
                failures.push(Failure {
                    check: format!("header[{key}]"),
                    expected: expected_val.clone(),
                    actual: actual.to_string(),
                });
            }
            _ => {}
        }
    }

    // body_contains 検証
    for needle in &expect.body_contains {
        if !response.body.contains(needle.as_str()) {
            failures.push(Failure {
                check: "body_contains".to_string(),
                expected: needle.clone(),
                actual: format!("(not found in {} bytes)", response.body.len()),
            });
        }
    }

    // body_not_contains 検証
    for needle in &expect.body_not_contains {
        if response.body.contains(needle.as_str()) {
            failures.push(Failure {
                check: "body_not_contains".to_string(),
                expected: format!("not contain: {needle}"),
                actual: "(found)".to_string(),
            });
        }
    }

    // body_matches 検証（正規表現）
    for pattern in &expect.body_matches {
        match Regex::new(pattern) {
            Ok(re) => {
                if !re.is_match(&response.body) {
                    failures.push(Failure {
                        check: "body_matches".to_string(),
                        expected: pattern.clone(),
                        actual: format!("(no match in {} bytes)", response.body.len()),
                    });
                }
            }
            Err(e) => {
                failures.push(Failure {
                    check: "body_matches (invalid pattern)".to_string(),
                    expected: pattern.clone(),
                    actual: format!("(regex compile error: {e})"),
                });
            }
        }
    }

    // body_not_matches 検証（正規表現）
    for pattern in &expect.body_not_matches {
        match Regex::new(pattern) {
            Ok(re) => {
                if re.is_match(&response.body) {
                    failures.push(Failure {
                        check: "body_not_matches".to_string(),
                        expected: format!("not match: {pattern}"),
                        actual: "(matched)".to_string(),
                    });
                }
            }
            Err(e) => {
                failures.push(Failure {
                    check: "body_not_matches (invalid pattern)".to_string(),
                    expected: pattern.clone(),
                    actual: format!("(regex compile error: {e})"),
                });
            }
        }
    }

    failures
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::types::StatusExpect;

    use super::*;

    fn make_response(status: u16, headers: &[(&str, &str)], body: &str) -> HttpResponse {
        HttpResponse {
            status,
            http_version: "HTTP/1.1".to_string(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            actual_request_headers: HashMap::new(),
            body: body.to_string(),
            charset: None,
            elapsed_ms: 100,
            size_bytes: body.len(),
        }
    }

    #[test]
    fn no_expect_always_pass() {
        let resp = make_response(500, &[], "");
        assert!(check(None, &resp).is_empty());
    }

    #[test]
    fn status_pass() {
        let expect = ExpectDef {
            status: Some(StatusExpect::Single(200)),
            headers: HashMap::new(),
            body_contains: Vec::new(),
            body_not_contains: Vec::new(),
            body_matches: Vec::new(),
            body_not_matches: Vec::new(),
        };
        let resp = make_response(200, &[], "");
        assert!(check(Some(&expect), &resp).is_empty());
    }

    #[test]
    fn status_fail() {
        let expect = ExpectDef {
            status: Some(StatusExpect::Single(200)),
            headers: HashMap::new(),
            body_contains: Vec::new(),
            body_not_contains: Vec::new(),
            body_matches: Vec::new(),
            body_not_matches: Vec::new(),
        };
        let resp = make_response(500, &[], "");
        let failures = check(Some(&expect), &resp);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].check, "status");
    }

    #[test]
    fn status_multiple_pass() {
        let expect = ExpectDef {
            status: Some(StatusExpect::Multiple(vec![200, 404])),
            headers: HashMap::new(),
            body_contains: Vec::new(),
            body_not_contains: Vec::new(),
            body_matches: Vec::new(),
            body_not_matches: Vec::new(),
        };
        let resp = make_response(404, &[], "");
        assert!(check(Some(&expect), &resp).is_empty());
    }

    #[test]
    fn header_pass() {
        let expect = ExpectDef {
            status: None,
            headers: HashMap::from([("content-type".to_string(), "text/csv".to_string())]),
            body_contains: Vec::new(),
            body_not_contains: Vec::new(),
            body_matches: Vec::new(),
            body_not_matches: Vec::new(),
        };
        let resp = make_response(200, &[("Content-Type", "text/csv; charset=utf-8")], "");
        assert!(check(Some(&expect), &resp).is_empty());
    }

    #[test]
    fn header_fail() {
        let expect = ExpectDef {
            status: None,
            headers: HashMap::from([("content-type".to_string(), "text/csv".to_string())]),
            body_contains: Vec::new(),
            body_not_contains: Vec::new(),
            body_matches: Vec::new(),
            body_not_matches: Vec::new(),
        };
        let resp = make_response(200, &[("Content-Type", "application/json")], "");
        let failures = check(Some(&expect), &resp);
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn header_missing() {
        let expect = ExpectDef {
            status: None,
            headers: HashMap::from([("x-custom".to_string(), "foo".to_string())]),
            body_contains: Vec::new(),
            body_not_contains: Vec::new(),
            body_matches: Vec::new(),
            body_not_matches: Vec::new(),
        };
        let resp = make_response(200, &[], "");
        let failures = check(Some(&expect), &resp);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].actual.contains("missing"));
    }

    #[test]
    fn body_contains_pass() {
        let expect = ExpectDef {
            status: None,
            headers: HashMap::new(),
            body_contains: vec!["会社名".to_string()],
            body_not_contains: Vec::new(),
            body_matches: Vec::new(),
            body_not_matches: Vec::new(),
        };
        let resp = make_response(200, &[], "会社名,メール");
        assert!(check(Some(&expect), &resp).is_empty());
    }

    #[test]
    fn body_contains_fail() {
        let expect = ExpectDef {
            status: None,
            headers: HashMap::new(),
            body_contains: vec!["会社名".to_string()],
            body_not_contains: Vec::new(),
            body_matches: Vec::new(),
            body_not_matches: Vec::new(),
        };
        let resp = make_response(200, &[], "no match");
        assert_eq!(check(Some(&expect), &resp).len(), 1);
    }

    #[test]
    fn body_not_contains_pass() {
        let expect = ExpectDef {
            status: None,
            headers: HashMap::new(),
            body_contains: Vec::new(),
            body_not_contains: vec!["error".to_string()],
            body_matches: Vec::new(),
            body_not_matches: Vec::new(),
        };
        let resp = make_response(200, &[], "ok");
        assert!(check(Some(&expect), &resp).is_empty());
    }

    #[test]
    fn body_not_contains_fail() {
        let expect = ExpectDef {
            status: None,
            headers: HashMap::new(),
            body_contains: Vec::new(),
            body_not_contains: vec!["error".to_string()],
            body_matches: Vec::new(),
            body_not_matches: Vec::new(),
        };
        let resp = make_response(200, &[], "error occurred");
        assert_eq!(check(Some(&expect), &resp).len(), 1);
    }

    #[test]
    fn body_matches_pass() {
        let expect = ExpectDef {
            status: None,
            headers: HashMap::new(),
            body_contains: Vec::new(),
            body_not_contains: Vec::new(),
            body_matches: vec![r"beforeSystemDate=\d{4}-\d{2}-\d{2}".to_string()],
            body_not_matches: Vec::new(),
        };
        let resp = make_response(200, &[], "beforeSystemDate=2026-07-09");
        assert!(check(Some(&expect), &resp).is_empty());
    }

    #[test]
    fn body_matches_fail() {
        let expect = ExpectDef {
            status: None,
            headers: HashMap::new(),
            body_contains: Vec::new(),
            body_not_contains: Vec::new(),
            body_matches: vec![r"beforeSystemDate=\d{4}-\d{2}-\d{2}".to_string()],
            body_not_matches: Vec::new(),
        };
        let resp = make_response(200, &[], "no date here");
        let failures = check(Some(&expect), &resp);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].check, "body_matches");
    }

    #[test]
    fn body_matches_invalid_pattern() {
        let expect = ExpectDef {
            status: None,
            headers: HashMap::new(),
            body_contains: Vec::new(),
            body_not_contains: Vec::new(),
            body_matches: vec!["(unclosed".to_string()],
            body_not_matches: Vec::new(),
        };
        let resp = make_response(200, &[], "anything");
        let failures = check(Some(&expect), &resp);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].check, "body_matches (invalid pattern)");
    }

    #[test]
    fn body_not_matches_pass() {
        let expect = ExpectDef {
            status: None,
            headers: HashMap::new(),
            body_contains: Vec::new(),
            body_not_contains: Vec::new(),
            body_matches: Vec::new(),
            body_not_matches: vec![r"^ERROR:".to_string()],
        };
        let resp = make_response(200, &[], "ok: all good");
        assert!(check(Some(&expect), &resp).is_empty());
    }

    #[test]
    fn body_not_matches_fail() {
        let expect = ExpectDef {
            status: None,
            headers: HashMap::new(),
            body_contains: Vec::new(),
            body_not_contains: Vec::new(),
            body_matches: Vec::new(),
            body_not_matches: vec![r"^ERROR:".to_string()],
        };
        let resp = make_response(200, &[], "ERROR: something broke");
        let failures = check(Some(&expect), &resp);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].check, "body_not_matches");
    }

    #[test]
    fn body_not_matches_invalid_pattern() {
        let expect = ExpectDef {
            status: None,
            headers: HashMap::new(),
            body_contains: Vec::new(),
            body_not_contains: Vec::new(),
            body_matches: Vec::new(),
            body_not_matches: vec!["[unclosed".to_string()],
        };
        let resp = make_response(200, &[], "anything");
        let failures = check(Some(&expect), &resp);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].check, "body_not_matches (invalid pattern)");
    }
}
