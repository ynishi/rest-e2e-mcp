use std::collections::HashMap;
use std::path::Path;

use std::time::{Duration, Instant};

use crate::secret::{
    CliBackend, DEADLINE_REASON, NOT_ALLOWED_REASON, RESOLVE_DEADLINE, SECRET_VAR_NAMES,
    SecretAllowlist, SecretBackend, is_reference,
};
use crate::types::{ResolveFailure, ResolvedVars, VarSource, VarTrace};

/// 4層の変数ソースを統合して解決する。
///
/// 優先順位（高い順）:
/// 1. runtime_vars（MCPツールパラメータ）
/// 2. suite_vars（YAMLファイル内の variables）
/// 3. dotenv（.env ファイル）
/// 4. OS環境変数
///
/// 許可リストが空のため、シークレット参照（`keychain://` / `op://`）は
/// 解決されず失敗になる。サーバは [`resolve_variables_with`] を使う。
pub fn resolve_variables(
    runtime_vars: &HashMap<String, String>,
    suite_vars: &HashMap<String, String>,
    env_file: Option<&Path>,
) -> ResolvedVars {
    resolve_variables_with(
        runtime_vars,
        suite_vars,
        env_file,
        &CliBackend::default(),
        &SecretAllowlist::default(),
        ResolveMode::Resolve,
    )
}

/// 参照の扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveMode {
    /// 許可リストに一致する参照をバックエンドで解決する。
    Resolve,
    /// 参照を解決せず参照文字列のまま返す（バックエンドを呼ばない。`parse` 用）。
    ReferencesAsIs,
}

/// [`resolve_variables`] のバックエンド・許可リスト注入版。
///
/// 勝者の値がシークレット参照で、許可リスト（`--secret-ref-allow`）に一致する
/// 場合のみ解決する。由来ソース（runtime / suite / .env / OS環境変数）は問わない。
pub fn resolve_variables_with(
    runtime_vars: &HashMap<String, String>,
    suite_vars: &HashMap<String, String>,
    env_file: Option<&Path>,
    backend: &dyn SecretBackend,
    allow: &SecretAllowlist,
    mode: ResolveMode,
) -> ResolvedVars {
    resolve_variables_in(
        runtime_vars,
        suite_vars,
        env_file,
        &|n| std::env::var(n).ok(),
        backend,
        allow,
        mode,
        RESOLVE_DEADLINE,
    )
}

/// OS環境変数の参照元を注入できる実装（テストをホスト環境から独立させる）。
/// OS環境変数は列挙せず、他のソースに現れた名前についてのみ参照する。
///
/// `budget` は1回の解決で全参照に使える合計時間。超過後の参照はバックエンドを
/// 呼ばずに `DEADLINE_REASON` で失敗する。
#[allow(clippy::too_many_arguments)]
pub fn resolve_variables_in(
    runtime_vars: &HashMap<String, String>,
    suite_vars: &HashMap<String, String>,
    env_file: Option<&Path>,
    os_env: &dyn Fn(&str) -> Option<String>,
    backend: &dyn SecretBackend,
    allow: &SecretAllowlist,
    mode: ResolveMode,
    budget: Duration,
) -> ResolvedVars {
    let deadline = crate::secret::deadline_after(budget);
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
    let mut failures = Vec::new();

    for name in &all_names {
        let mut found_in: Vec<(VarSource, String)> = Vec::new();

        // 低い優先順位から積む
        if let Some(val) = os_env(name) {
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
            let mut final_value = value.clone();
            let mut reference = None;
            // 参照文字列そのものはシークレットではない
            let mut secret = SECRET_VAR_NAMES.contains(&name.as_str()) && !is_reference(value);

            if is_reference(value) {
                reference = Some(value.clone());
                if mode == ResolveMode::Resolve {
                    let outcome = if !allow.is_allowed(value) {
                        Err(NOT_ALLOWED_REASON.to_string())
                    } else if Instant::now() >= deadline {
                        Err(DEADLINE_REASON.to_string())
                    } else {
                        let left = deadline.saturating_duration_since(Instant::now());
                        backend.resolve_within(value, left).and_then(|r| {
                            if r.is_empty() {
                                Err("secret backend returned an empty value".to_string())
                            } else {
                                Ok(r)
                            }
                        })
                    };
                    match outcome {
                        Ok(resolved) => {
                            final_value = resolved;
                            secret = true;
                        }
                        Err(reason) => {
                            failures.push(ResolveFailure::SecretResolutionFailed {
                                name: name.clone(),
                                reference: value.clone(),
                                reason,
                            });
                            continue;
                        }
                    }
                }
            }

            vars.insert(name.clone(), final_value.clone());
            traces.push(VarTrace {
                name: name.clone(),
                value: final_value,
                source: source.clone(),
                found_in: found_in.clone(),
                secret,
                reference,
            });
        }
    }

    ResolvedVars {
        vars,
        traces,
        failures,
    }
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
    substitute_with_check_failed(input, vars, &[])
}

/// [`substitute_with_check`] に「解決失敗した変数名」を渡す版。
///
/// `failed` に含まれる名前への参照は、`${NAME:default}` のようにデフォルト値で
/// 置換される形でも、また展開途中のネスト変数経由でも、必ず `unresolved` に含まれる。
pub fn substitute_with_check_failed(
    input: &str,
    vars: &HashMap<String, String>,
    failed: &[String],
) -> SubstituteResult {
    const MAX_DEPTH: usize = 10;
    let mut result = input.to_string();
    let mut failed_hits: Vec<String> = Vec::new();
    for _ in 0..MAX_DEPTH {
        let mut seen = Vec::new();
        let next = substitute_inner(&result, vars, Some(&mut seen), failed);
        failed_hits.extend(seen.into_iter().filter(|n| failed.contains(n)));
        if next == result {
            break;
        }
        result = next;
    }
    // 安定状態から未解決変数を収集
    let mut unresolved = Vec::new();
    let _ = substitute_inner(&result, vars, Some(&mut unresolved), failed);
    unresolved.extend(failed_hits);
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
        let next = substitute_inner(&result, vars, None, &[]);
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
    failed: &[String],
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
                    // 解決失敗した変数はデフォルト値で隠さず、未解決として記録する
                    if failed.iter().any(|f| f == var_name)
                        && let Some(ref mut out) = unresolved_out
                    {
                        out.push(var_name.to_string());
                    }
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

    // ---- secret references -------------------------------------------------

    use std::sync::Mutex;

    struct FakeBackend {
        reply: Result<String, String>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeBackend {
        fn ok(v: &str) -> Self {
            Self {
                reply: Ok(v.to_string()),
                calls: Default::default(),
            }
        }
        fn err() -> Self {
            Self {
                reply: Err("backend failed".to_string()),
                calls: Default::default(),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SecretBackend for FakeBackend {
        fn resolve(&self, reference: &str) -> Result<String, String> {
            self.calls.lock().unwrap().push(reference.to_string());
            self.reply.clone()
        }
    }

    fn dotenv_file(content: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    fn no_os(_: &str) -> Option<String> {
        None
    }

    fn allow(patterns: &[&str]) -> SecretAllowlist {
        SecretAllowlist::new(patterns.iter().map(|p| p.to_string()))
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_h(
        runtime: &HashMap<String, String>,
        suite: &HashMap<String, String>,
        env_file: Option<&Path>,
        b: &dyn SecretBackend,
        al: &SecretAllowlist,
        mode: ResolveMode,
    ) -> ResolvedVars {
        resolve_variables_in(
            runtime,
            suite,
            env_file,
            &no_os,
            b,
            al,
            mode,
            RESOLVE_DEADLINE,
        )
    }

    #[test]
    fn allowlisted_dotenv_reference_is_resolved_and_marked_secret() {
        let f = dotenv_file("TOKEN=keychain://svc/acct\nPLAIN=hello\n");
        let b = FakeBackend::ok("resolved-secret-value");
        let r = resolve_h(
            &HashMap::new(),
            &HashMap::new(),
            Some(f.path()),
            &b,
            &allow(&["keychain://svc/*"]),
            ResolveMode::Resolve,
        );
        assert_eq!(r.vars["TOKEN"], "resolved-secret-value");
        assert_eq!(r.vars["PLAIN"], "hello");
        assert_eq!(b.calls(), vec!["keychain://svc/acct".to_string()]);
        let t = r.traces.iter().find(|t| t.name == "TOKEN").unwrap();
        assert!(t.secret);
        assert_eq!(t.reference.as_deref(), Some("keychain://svc/acct"));
        assert!(t.found_in.iter().all(|(_, v)| v != "resolved-secret-value"));
        assert!(!r.traces.iter().find(|t| t.name == "PLAIN").unwrap().secret);
        assert!(r.failures.is_empty());
    }

    #[test]
    fn allowlisted_suite_and_runtime_references_resolve_regardless_of_source() {
        let b = FakeBackend::ok("value-from-store");
        let suite = make_vars(&[("A", "keychain://svc/a")]);
        let runtime = make_vars(&[("B", "op://Dev/i/f")]);
        let r = resolve_h(
            &runtime,
            &suite,
            None,
            &b,
            &allow(&["keychain://svc/*", "op://Dev/*"]),
            ResolveMode::Resolve,
        );
        assert_eq!(r.vars["A"], "value-from-store");
        assert_eq!(r.vars["B"], "value-from-store");
        assert_eq!(b.calls().len(), 2);
        assert!(r.traces.iter().all(|t| t.secret));
    }

    #[test]
    fn non_allowlisted_reference_fails_without_calling_backend() {
        let f = dotenv_file("A=keychain://evil/acct\n");
        let b = FakeBackend::ok("should-never-be-used");
        let suite = make_vars(&[("S", "op://Prod/i/f")]);
        let runtime = make_vars(&[("R", "keychain://svc/other")]);
        for al in [allow(&[]), allow(&["keychain://svc/mine"])] {
            let r = resolve_h(
                &runtime,
                &suite,
                Some(f.path()),
                &b,
                &al,
                ResolveMode::Resolve,
            );
            assert!(b.calls().is_empty());
            assert!(r.vars.is_empty(), "literal reference must not be exposed");
            assert_eq!(r.failures.len(), 3);
            for f in &r.failures {
                let ResolveFailure::SecretResolutionFailed { reason, .. } = f;
                assert_eq!(reason, "not allowed by --secret-ref-allow");
            }
        }
    }

    #[test]
    fn os_env_reference_follows_same_allowlist_rule() {
        // OS env is the lowest-priority source and only consulted for names
        // defined elsewhere; a runtime literal therefore always wins over it.
        let os = |n: &str| (n == "T").then(|| "keychain://svc/os".to_string());
        let b = FakeBackend::ok("v-from-store");
        let r = resolve_variables_in(
            &HashMap::new(),
            &HashMap::new(),
            None,
            &os,
            &b,
            &allow(&["keychain://svc/*"]),
            ResolveMode::Resolve,
            RESOLVE_DEADLINE,
        );
        // not enumerated -> not reachable at all
        assert!(r.vars.is_empty() && b.calls().is_empty());
        let r = resolve_variables_in(
            &make_vars(&[("T", "literal")]),
            &HashMap::new(),
            None,
            &os,
            &b,
            &allow(&["keychain://svc/*"]),
            ResolveMode::Resolve,
            RESOLVE_DEADLINE,
        );
        assert_eq!(r.vars["T"], "literal");
        assert!(b.calls().is_empty());
    }

    #[test]
    fn higher_priority_literal_beats_dotenv_reference() {
        let f = dotenv_file("K=keychain://svc/acct\n");
        let b = FakeBackend::ok("zzzzzzzzzz");
        let r = resolve_h(
            &HashMap::new(),
            &make_vars(&[("K", "literal")]),
            Some(f.path()),
            &b,
            &allow(&["*"]),
            ResolveMode::Resolve,
        );
        assert!(b.calls().is_empty());
        assert_eq!(r.vars["K"], "literal");
    }

    #[test]
    fn backend_error_yields_failure_not_literal_or_empty() {
        let f = dotenv_file("API_KEY=op://v/i/f\n");
        let b = FakeBackend::err();
        let r = resolve_h(
            &HashMap::new(),
            &HashMap::new(),
            Some(f.path()),
            &b,
            &allow(&["op://*"]),
            ResolveMode::Resolve,
        );
        assert!(!r.vars.contains_key("API_KEY"));
        assert_eq!(
            r.failures,
            vec![ResolveFailure::SecretResolutionFailed {
                name: "API_KEY".into(),
                reference: "op://v/i/f".into(),
                reason: "backend failed".into(),
            }]
        );
    }

    #[test]
    fn empty_backend_output_is_failure() {
        let f = dotenv_file("X=op://v/i/f\n");
        let b = FakeBackend::ok("");
        let r = resolve_h(
            &HashMap::new(),
            &HashMap::new(),
            Some(f.path()),
            &b,
            &allow(&["*"]),
            ResolveMode::Resolve,
        );
        assert!(!r.vars.contains_key("X"));
        assert_eq!(r.failures.len(), 1);
    }

    #[test]
    fn references_as_is_never_calls_backend_and_is_not_secret() {
        let b = FakeBackend::ok("never");
        let runtime = make_vars(&[
            ("API_KEY", "op://v/i/f"),
            ("BEARER_TOKEN", "literal-bearer-1"),
        ]);
        let r = resolve_h(
            &runtime,
            &HashMap::new(),
            None,
            &b,
            &allow(&["*"]),
            ResolveMode::ReferencesAsIs,
        );
        assert!(b.calls().is_empty());
        assert_eq!(r.vars["API_KEY"], "op://v/i/f");
        let t = r.traces.iter().find(|t| t.name == "API_KEY").unwrap();
        assert!(!t.secret, "reference string is not a secret");
        assert_eq!(t.reference.as_deref(), Some("op://v/i/f"));
        // literal bearer is still secret
        assert_eq!(r.secret_values(), vec!["literal-bearer-1".to_string()]);
    }

    #[test]
    fn bearer_var_names_are_secret_regardless_of_origin() {
        let r = resolve_h(
            &make_vars(&[("BEARER_TOKEN", "literal-token-1234")]),
            &HashMap::new(),
            None,
            &FakeBackend::err(),
            &allow(&[]),
            ResolveMode::Resolve,
        );
        let t = &r.traces[0];
        assert!(t.secret);
        assert!(t.reference.is_none());
        assert_eq!(r.secret_values(), vec!["literal-token-1234".to_string()]);
    }

    #[test]
    fn failed_variable_with_default_is_reported_unresolved() {
        let vars = make_vars(&[("OK", "fine")]);
        let failed = vec!["TOK".to_string()];
        for text in [
            "a=${TOK:fallback}",
            "a={{TOK}}",
            "a=${TOK}",
            "a=$TOK",
            "a=${ TOK : fallback }",
        ] {
            let r = substitute_with_check_failed(text, &vars, &failed);
            assert_eq!(r.unresolved, vec!["TOK".to_string()], "{text}");
        }
        // nested: a variable whose value uses the failed var with a default
        let nested = make_vars(&[("OUTER", "x-${TOK:dflt}")]);
        let r = substitute_with_check_failed("{{OUTER}}", &nested, &failed);
        assert_eq!(r.unresolved, vec!["TOK".to_string()]);
        // untouched when no failed var is referenced; other defaults still work
        let r = substitute_with_check_failed("${NOPE:d} {{OK}}", &vars, &failed);
        assert!(r.unresolved.is_empty());
        assert_eq!(r.text, "d fine");
    }

    struct SlowBackend {
        delay: Duration,
        calls: Mutex<Vec<String>>,
    }

    impl SecretBackend for SlowBackend {
        fn resolve(&self, reference: &str) -> Result<String, String> {
            self.calls.lock().unwrap().push(reference.to_string());
            std::thread::sleep(self.delay);
            Ok("slow-resolved-value".to_string())
        }
    }

    #[test]
    fn overall_deadline_fails_remaining_references_without_calling_backend() {
        let b = SlowBackend {
            delay: Duration::from_millis(80),
            calls: Default::default(),
        };
        let suite = make_vars(&[
            ("A", "keychain://svc/a"),
            ("B", "keychain://svc/b"),
            ("C", "keychain://svc/c"),
        ]);
        let r = resolve_variables_in(
            &HashMap::new(),
            &suite,
            None,
            &no_os,
            &b,
            &allow(&["keychain://svc/*"]),
            ResolveMode::Resolve,
            Duration::from_millis(50),
        );
        assert_eq!(b.calls.lock().unwrap().len(), 1, "only A reaches backend");
        assert_eq!(r.vars["A"], "slow-resolved-value");
        let failed: Vec<_> = r
            .failures
            .iter()
            .map(|f| {
                let ResolveFailure::SecretResolutionFailed { name, reason, .. } = f;
                (name.as_str(), reason.as_str())
            })
            .collect();
        assert_eq!(
            failed,
            vec![
                ("B", "overall secret resolution deadline exceeded"),
                ("C", "overall secret resolution deadline exceeded")
            ]
        );
    }

    #[test]
    fn huge_budget_does_not_panic() {
        let b = FakeBackend::ok("resolved-secret-value");
        let suite = make_vars(&[("A", "keychain://svc/a")]);
        let r = resolve_variables_in(
            &HashMap::new(),
            &suite,
            None,
            &no_os,
            &b,
            &allow(&["keychain://svc/*"]),
            ResolveMode::Resolve,
            Duration::MAX,
        );
        assert_eq!(r.vars["A"], "resolved-secret-value");
    }
}
