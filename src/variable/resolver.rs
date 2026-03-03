use std::collections::HashMap;
use std::path::Path;

use crate::types::{ResolvedVars, VarSource, VarTrace};

/// 4層の変数ソースを統合して解決する。
///
/// 優先順位（高→低）:
/// 1. runtime（run の variables パラメータ）
/// 2. suite_vars（YAMLファイル内の variables）
/// 3. dotenv（.env ファイル）
/// 4. OS環境変数
pub fn resolve_variables(
    runtime_vars: &HashMap<String, String>,
    suite_vars: &HashMap<String, String>,
    env_file: Option<&Path>,
) -> ResolvedVars {
    // .env読み込み
    let dotenv_vars = load_dotenv(env_file);

    // 全変数名を収集（BTreeSetで重複除去 + ソート済み）
    let mut name_set = std::collections::BTreeSet::new();
    name_set.extend(runtime_vars.keys().cloned());
    name_set.extend(suite_vars.keys().cloned());
    name_set.extend(dotenv_vars.keys().cloned());
    // OS環境変数は明示的に列挙しない（他のソースで参照された名前のみ追加）
    let all_names: Vec<String> = name_set.into_iter().collect();

    let mut vars = HashMap::new();
    let mut traces = Vec::new();

    for name in &all_names {
        let mut found_in: Vec<(VarSource, String)> = Vec::new();

        // 低い優先順位から積む
        if let Ok(val) = std::env::var(name) {
            found_in.push((VarSource::OsEnv, val));
        }
        if let Some(val) = dotenv_vars.get(name) {
            found_in.push((VarSource::DotEnv, val.clone()));
        }
        if let Some(val) = suite_vars.get(name) {
            found_in.push((VarSource::SuiteFile, val.clone()));
        }
        if let Some(val) = runtime_vars.get(name) {
            found_in.push((VarSource::Runtime, val.clone()));
        }

        if let Some((source, value)) = found_in.last() {
            vars.insert(name.clone(), value.clone());
            traces.push(VarTrace {
                name: name.clone(),
                value: value.clone(),
                source: source.clone(),
                found_in: found_in.clone(),
            });
        }
    }

    ResolvedVars { vars, traces }
}

/// 変数展開の結果。
pub struct SubstituteResult {
    /// 展開後のテキスト。
    pub text: String,
    /// 未解決の変数名リスト。
    pub unresolved: Vec<String>,
}

/// 文字列中の変数参照を展開し、未解決変数も返す（ネスト変数を再帰的に解決）。
pub fn substitute_with_check(input: &str, vars: &HashMap<String, String>) -> SubstituteResult {
    const MAX_DEPTH: usize = 10;
    let mut result = input.to_string();
    for _ in 0..MAX_DEPTH {
        let next = substitute_inner(&result, vars, None);
        if next == result {
            break;
        }
        result = next;
    }
    // 安定状態から未解決変数を収集
    let mut unresolved = Vec::new();
    let _ = substitute_inner(&result, vars, Some(&mut unresolved));
    unresolved.sort();
    unresolved.dedup();
    SubstituteResult {
        text: result,
        unresolved,
    }
}

/// 文字列中の変数参照を展開する（ネスト変数を再帰的に解決）。
///
/// 対応構文:
/// - `{{VARIABLE_NAME}}` — テンプレート変数
/// - `${ENV_VAR}` — ブレース付き変数参照
/// - `${ENV_VAR:default}` — デフォルト値付き（ブレース構文のみ）
/// - `$ENV_VAR` — 変数直接参照（単語境界で区切り、デフォルト値なし）
pub fn substitute(input: &str, vars: &HashMap<String, String>) -> String {
    const MAX_DEPTH: usize = 10;
    let mut result = input.to_string();
    for _ in 0..MAX_DEPTH {
        let next = substitute_inner(&result, vars, None);
        if next == result {
            break;
        }
        result = next;
    }
    result
}

fn substitute_inner(
    input: &str,
    vars: &HashMap<String, String>,
    mut unresolved_out: Option<&mut Vec<String>>,
) -> String {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // {{VAR}} 構文
        if i + 1 < chars.len()
            && chars[i] == '{'
            && chars[i + 1] == '{'
            && let Some(end) = find_closing_braces(&chars, i + 2)
        {
            let var_name: String = chars[i + 2..end].iter().collect();
            let var_name = var_name.trim();
            if let Some(val) = vars.get(var_name) {
                result.push_str(val);
            } else {
                // 未解決の変数はそのまま残す
                let original: String = chars[i..end + 2].iter().collect();
                result.push_str(&original);
                if let Some(ref mut out) = unresolved_out {
                    out.push(var_name.to_string());
                }
            }
            i = end + 2;
            continue;
        }

        // ${VAR} or ${VAR:default} 構文
        if i + 1 < chars.len()
            && chars[i] == '$'
            && chars[i + 1] == '{'
            && let Some(end) = find_closing_brace(&chars, i + 2)
        {
            let inner: String = chars[i + 2..end].iter().collect();
            let inner = inner.trim();

            // ${VAR:default} 構文
            if let Some((var_name, default_val)) = inner.split_once(':') {
                let var_name = var_name.trim();
                let default_val = default_val.trim();
                if let Some(val) = vars.get(var_name) {
                    result.push_str(val);
                } else {
                    result.push_str(default_val);
                }
            } else if let Some(val) = vars.get(inner) {
                result.push_str(val);
            } else {
                let original: String = chars[i..end + 1].iter().collect();
                result.push_str(&original);
                if let Some(ref mut out) = unresolved_out {
                    out.push(inner.to_string());
                }
            }
            i = end + 1;
            continue;
        }

        // $VAR 構文（bare dollar、デフォルト値なし）
        if chars[i] == '$'
            && i + 1 < chars.len()
            && (chars[i + 1].is_ascii_alphabetic() || chars[i + 1] == '_')
        {
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
            let var_name: String = chars[start..end].iter().collect();

            if let Some(val) = vars.get(&var_name) {
                result.push_str(val);
            } else {
                result.push('$');
                result.push_str(&var_name);
                if let Some(ref mut out) = unresolved_out {
                    out.push(var_name.clone());
                }
            }
            i = end;
            continue;
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

/// .envファイルを読み込む。
fn load_dotenv(path: Option<&Path>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(path) = path
        && path.exists()
        && let Ok(iter) = dotenvy::from_path_iter(path)
    {
        for (key, val) in iter.flatten() {
            map.insert(key, val);
        }
    }
    map
}

fn find_closing_braces(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start;
    while i + 1 < chars.len() {
        if chars[i] == '}' && chars[i + 1] == '}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_closing_brace(chars: &[char], start: usize) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        if chars[i] == '}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn substitute_double_braces() {
        let vars = make_vars(&[("BASE_URL", "https://example.com")]);
        assert_eq!(
            substitute("{{BASE_URL}}/api", &vars),
            "https://example.com/api"
        );
    }

    #[test]
    fn substitute_dollar_brace() {
        let vars = make_vars(&[("API_KEY", "secret")]);
        assert_eq!(substitute("Bearer ${API_KEY}", &vars), "Bearer secret");
    }

    #[test]
    fn substitute_dollar_bare() {
        let vars = make_vars(&[("HOST", "localhost")]);
        assert_eq!(
            substitute("http://$HOST:8080", &vars),
            "http://localhost:8080"
        );
    }

    #[test]
    fn substitute_default_value_brace() {
        let vars: HashMap<String, String> = HashMap::new();
        assert_eq!(substitute("${MISSING:fallback}", &vars), "fallback");
    }

    #[test]
    fn substitute_default_not_used_when_present_brace() {
        let vars = make_vars(&[("PORT", "9090")]);
        assert_eq!(substitute("${PORT:8080}", &vars), "9090");
    }

    #[test]
    fn substitute_bare_dollar_no_default() {
        // bare $VAR は :default を消費しない（URL port との衝突回避）
        let vars = make_vars(&[("PORT", "9090")]);
        assert_eq!(substitute("$PORT:8080", &vars), "9090:8080");
    }

    #[test]
    fn substitute_unresolved_preserved() {
        let vars: HashMap<String, String> = HashMap::new();
        assert_eq!(substitute("{{UNKNOWN}}", &vars), "{{UNKNOWN}}");
    }

    #[test]
    fn resolve_priority() {
        // runtime > suite > dotenv > OS
        let runtime = make_vars(&[("KEY", "from_runtime")]);
        let suite = make_vars(&[("KEY", "from_suite"), ("OTHER", "suite_val")]);

        let resolved = resolve_variables(&runtime, &suite, None);
        assert_eq!(resolved.vars.get("KEY").unwrap(), "from_runtime");
        assert_eq!(resolved.vars.get("OTHER").unwrap(), "suite_val");

        // トレース確認
        let key_trace = resolved.traces.iter().find(|t| t.name == "KEY").unwrap();
        assert_eq!(key_trace.source, VarSource::Runtime);
        assert!(key_trace.found_in.len() >= 2);
    }

    #[test]
    fn substitute_with_check_detects_unresolved_double_braces() {
        let vars = make_vars(&[("FOO", "abc")]);
        let result = substitute_with_check("{{FOO}}/{{BAR}}/{{BAZ}}", &vars);
        assert_eq!(result.text, "abc/{{BAR}}/{{BAZ}}");
        assert_eq!(result.unresolved, vec!["BAR", "BAZ"]);
    }

    #[test]
    fn substitute_with_check_detects_unresolved_dollar_brace() {
        let vars: HashMap<String, String> = HashMap::new();
        let result = substitute_with_check("${MISSING}", &vars);
        assert_eq!(result.text, "${MISSING}");
        assert_eq!(result.unresolved, vec!["MISSING"]);
    }

    #[test]
    fn substitute_with_check_detects_unresolved_bare_dollar() {
        let vars: HashMap<String, String> = HashMap::new();
        let result = substitute_with_check("$BARE_VAR", &vars);
        assert_eq!(result.text, "$BARE_VAR");
        assert_eq!(result.unresolved, vec!["BARE_VAR"]);
    }

    #[test]
    fn substitute_with_check_no_unresolved() {
        let vars = make_vars(&[("HOST", "localhost"), ("PORT", "8080")]);
        let result = substitute_with_check("{{HOST}}:${PORT}", &vars);
        assert_eq!(result.text, "localhost:8080");
        assert!(result.unresolved.is_empty());
    }

    #[test]
    fn substitute_with_check_deduplicates() {
        let vars: HashMap<String, String> = HashMap::new();
        let result = substitute_with_check("{{X}}/{{X}}/{{X}}", &vars);
        assert_eq!(result.unresolved, vec!["X"]);
    }

    #[test]
    fn substitute_with_check_default_value_not_unresolved() {
        let vars: HashMap<String, String> = HashMap::new();
        let result = substitute_with_check("${MISSING:fallback}", &vars);
        assert_eq!(result.text, "fallback");
        assert!(result.unresolved.is_empty());
    }

    #[test]
    fn substitute_nested_variables() {
        let vars = make_vars(&[
            ("ASSET_ID", "{{ASSET_ID_GROUP}}"),
            ("ASSET_ID_GROUP", "abc123"),
        ]);
        assert_eq!(substitute("{{ASSET_ID}}", &vars), "abc123");
    }

    #[test]
    fn substitute_nested_three_levels() {
        let vars = make_vars(&[("A", "{{B}}"), ("B", "{{C}}"), ("C", "final_value")]);
        assert_eq!(substitute("{{A}}", &vars), "final_value");
    }

    #[test]
    fn substitute_nested_circular_stops() {
        let vars = make_vars(&[("A", "{{B}}"), ("B", "{{A}}")]);
        // 無限ループせず停止すること
        let result = substitute("{{A}}", &vars);
        assert!(result.contains("{{"));
    }

    #[test]
    fn substitute_with_check_nested_unresolved() {
        let vars = make_vars(&[("ASSET_ID", "{{MISSING}}")]);
        let result = substitute_with_check("{{ASSET_ID}}", &vars);
        assert_eq!(result.text, "{{MISSING}}");
        assert_eq!(result.unresolved, vec!["MISSING"]);
    }

    #[test]
    fn substitute_nested_mixed_syntax() {
        let vars = make_vars(&[("A", "$B"), ("B", "resolved")]);
        assert_eq!(substitute("{{A}}", &vars), "resolved");
    }
}
