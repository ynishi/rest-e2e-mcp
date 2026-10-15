//! Secret references (`keychain://`, `op://`) and output masking.
//!
//! A variable value may be a reference to a secret held in an external store.
//! A reference is resolved only if it matches the server command-line
//! allowlist (`--secret-ref-allow`, see [`SecretAllowlist`]), through a
//! [`SecretBackend`]; the resolved value is treated as secret and masked
//! (see [`Masker`]) before any output is produced.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::types::{HttpResponse, SuiteResult, TestResult};

/// Replacement marker for masked values.
pub const MASK: &str = "***";

/// Minimum length of a secret value for substring masking.
pub const MIN_MASK_LEN: usize = 8;

/// Variable names whose values are always treated as secret (auto-bearer inputs).
pub const SECRET_VAR_NAMES: &[&str] = &["API_KEY", "BEARER_TOKEN"];

/// Timeout for a single secret CLI invocation.
pub const BACKEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Overall time budget for resolving all references of one call.
pub const RESOLVE_DEADLINE: Duration = Duration::from_secs(30);

/// Failure reason for references not resolved before [`RESOLVE_DEADLINE`].
pub const DEADLINE_REASON: &str = "overall secret resolution deadline exceeded";

/// Failure reason used when a reference is not on the allowlist.
pub const NOT_ALLOWED_REASON: &str = "not allowed by --secret-ref-allow";

const SCHEMES: &[&str] = &["keychain://", "op://"];

/// True when `value` starts with a registered secret reference scheme.
pub fn is_reference(value: &str) -> bool {
    SCHEMES.iter().any(|s| value.starts_with(s))
}

/// Server-side allowlist of secret references (`--secret-ref-allow`).
///
/// Each entry is an exact reference or a pattern where `*` matches any run of
/// characters (including `/`). An empty allowlist allows nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecretAllowlist {
    patterns: Vec<String>,
}

impl SecretAllowlist {
    pub fn new<I: IntoIterator<Item = String>>(patterns: I) -> Self {
        Self {
            patterns: patterns.into_iter().collect(),
        }
    }

    pub fn is_allowed(&self, reference: &str) -> bool {
        self.patterns.iter().any(|p| glob_match(p, reference))
    }
}

/// Effective allowlist lines and warnings for startup output (print to stderr:
/// stdout is the MCP stdio transport). Pure so it can be tested.
pub fn allowlist_report(patterns: &[String]) -> Vec<String> {
    if patterns.is_empty() {
        return vec![
            "secret-ref-allow: (empty) - no secret reference will be resolved".to_string(),
        ];
    }
    let mut lines = vec![format!("secret-ref-allow: {} pattern(s)", patterns.len())];
    for p in patterns {
        lines.push(format!("  allow: {p}"));
        if p == "*" {
            lines.push(format!(
                "WARNING: pattern {p:?} allows EVERY secret reference of every scheme"
            ));
        } else if !SCHEMES.iter().any(|s| p.starts_with(s)) {
            lines.push(format!(
                "WARNING: pattern {p:?} does not start with a known scheme ({}); it may match across schemes",
                SCHEMES.join(", ")
            ));
        }
    }
    lines
}

/// Glob match where `*` matches any characters (including `/`); everything
/// else is literal. No other metacharacters.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let first = parts[0];
    let last = parts[parts.len() - 1];
    if text.len() < first.len() + last.len() || !text.starts_with(first) || !text.ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// Backend that turns a reference into its secret value.
///
/// The error string must never contain the resolved value or CLI stdout.
pub trait SecretBackend: Send + Sync {
    fn resolve(&self, reference: &str) -> Result<String, String>;

    /// Like [`resolve`](Self::resolve) but the call must not take longer than
    /// `limit` (used to honour [`RESOLVE_DEADLINE`]). The default ignores the
    /// limit; backends that can enforce it should override.
    fn resolve_within(&self, reference: &str, _limit: Duration) -> Result<String, String> {
        self.resolve(reference)
    }
}

/// Real backend: shells out to `security` (macOS) / `op` (1Password CLI).
/// Arguments are passed directly (no shell). Each call is bounded by `timeout`.
pub struct CliBackend {
    pub timeout: Duration,
}

impl Default for CliBackend {
    fn default() -> Self {
        Self {
            timeout: BACKEND_TIMEOUT,
        }
    }
}

/// Maximum accepted size of a secret CLI's stdout.
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Reason used when the CLI output exceeds [`MAX_OUTPUT_BYTES`].
const TOO_LARGE_REASON: &str = "secret CLI output too large";

/// `now + budget`, saturating to roughly one year ahead instead of panicking
/// when the addition would overflow `Instant` (huge `Duration` from a pub API).
pub fn deadline_after(budget: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(budget)
        .unwrap_or_else(|| now + Duration::from_secs(365 * 24 * 3600))
}

/// Run `cmd`, returning its stdout.
///
/// Unix: the child leads its own process group. The calling thread reads the
/// pipe (non-blocking, `poll`) until the deadline, so no helper thread exists
/// and nothing outlives this call. Exit is detected with `waitid(WNOWAIT)`
/// without reaping. On success the already-buffered output is drained
/// non-blockingly (a descendant holding the pipe is not waited for and the
/// group is not killed) and the child is reaped. On timeout / oversize the
/// group is killed while the zombie or child still holds the pid/pgid (no
/// pid-reuse race), then the child itself, then reaped. A child that exited
/// before the deadline is never reported as timed out. Output is capped at
/// [`MAX_OUTPUT_BYTES`]. stderr is discarded on purpose: it is never surfaced.
#[cfg(unix)]
fn run_cli(mut cmd: Command, timeout: Duration) -> Result<Vec<u8>, String> {
    use std::os::unix::io::AsRawFd;
    use std::os::unix::process::CommandExt;

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run secret CLI: {}", e.kind()))?;
    let Some(stdout) = child.stdout.take() else {
        kill_and_reap(&mut child);
        return Err("secret CLI stdout unavailable".to_string());
    };
    let fd = stdout.as_raw_fd();
    // SAFETY: plain fcntl calls on a descriptor owned by `stdout`.
    let nonblocking = unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        flags >= 0 && libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0
    };
    if !nonblocking {
        kill_and_reap(&mut child);
        return Err("failed to configure secret CLI pipe".to_string());
    }
    let Ok(pid) = i32::try_from(child.id()) else {
        kill_and_reap(&mut child);
        return Err("failed to identify secret CLI process".to_string());
    };

    let deadline = deadline_after(timeout);
    let mut buf: Vec<u8> = Vec::new();
    let mut eof = false;
    // Err(reason): stop and fail; Ok(()): child exited (not yet reaped).
    let mut outcome: Result<(), &str> = loop {
        // Exit is checked first so a completed child is never a timeout.
        match exited_without_reaping(pid) {
            Ok(true) => break Ok(()),
            Ok(false) => {}
            Err(()) => break Err("failed to wait for secret CLI"),
        }
        if !eof && let Err(msg) = read_available(fd, &mut buf, &mut eof) {
            break Err(msg);
        }
        let now = Instant::now();
        if now >= deadline {
            // A child that exited at the boundary counts as completed.
            break match exited_without_reaping(pid) {
                Ok(true) => Ok(()),
                Ok(false) => Err("secret CLI timed out"),
                Err(()) => Err("failed to wait for secret CLI"),
            };
        }
        let slice = deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(20));
        if eof {
            std::thread::sleep(slice.min(Duration::from_millis(10)));
        } else {
            poll_readable(fd, slice);
        }
    };

    if outcome.is_ok() {
        // The direct child exited: take what is already buffered in the pipe
        // without waiting for any descendant that still holds it open. The
        // process group is deliberately left alone on success.
        if !eof && let Err(msg) = read_available(fd, &mut buf, &mut eof) {
            outcome = Err(msg);
        }
    }
    if let Err(msg) = outcome {
        // Timeout / oversize / error: kill the group while the child still
        // owns pid/pgid, then the child itself (the group kill may fail or miss
        // if the child moved to another group), then reap.
        let _ = kill_group(pid);
        let _ = child.kill();
        let _ = child.wait();
        return Err(msg.to_string());
    }
    let status = child
        .wait()
        .map_err(|_| "failed to wait for secret CLI".to_string())?;
    drop(stdout);
    if !status.success() {
        return Err("secret CLI exited with non-zero status".to_string());
    }
    Ok(buf)
}

/// Read everything currently available without blocking.
#[cfg(unix)]
fn read_available(fd: i32, buf: &mut Vec<u8>, eof: &mut bool) -> Result<(), &'static str> {
    let mut chunk = [0u8; 4096];
    loop {
        // SAFETY: `chunk` is a valid writable buffer of the given length.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n > 0 {
            buf.extend_from_slice(&chunk[..n as usize]);
            if buf.len() > MAX_OUTPUT_BYTES {
                return Err(TOO_LARGE_REASON);
            }
        } else if n == 0 {
            *eof = true;
            return Ok(());
        } else {
            match std::io::Error::last_os_error().kind() {
                std::io::ErrorKind::WouldBlock => return Ok(()),
                std::io::ErrorKind::Interrupted => {}
                _ => return Err("failed to read secret CLI output"),
            }
        }
    }
}

/// Wait up to `limit` for the pipe to become readable (or hang up).
#[cfg(unix)]
fn poll_readable(fd: i32, limit: Duration) {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = limit.as_millis().clamp(1, 20) as i32;
    // SAFETY: one valid pollfd; errors (EINTR) just cause another loop turn.
    unsafe {
        libc::poll(&mut p, 1, ms);
    }
}

/// True once the child has exited; it is NOT reaped (`WNOWAIT`).
#[cfg(unix)]
fn exited_without_reaping(pid: i32) -> Result<bool, ()> {
    loop {
        // SAFETY: zeroed siginfo_t is a valid out-parameter for waitid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if r == 0 {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            // SAFETY: waitid filled `info`; si_pid is valid after success.
            let child_pid = unsafe { info.si_pid() };
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            let child_pid = info.si_pid;
            return Ok(child_pid != 0);
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return Err(());
        }
    }
}

/// SIGKILL the process group led by `pid`. ESRCH (group already gone) is fine;
/// any other error is reported (no errno detail is surfaced).
#[cfg(unix)]
fn kill_group(pid: i32) -> Result<(), ()> {
    // SAFETY: plain syscall; the group id equals the child pid because the
    // child was spawned with `process_group(0)` and has not been reaped.
    if unsafe { libc::kill(-pid, libc::SIGKILL) } == 0 {
        return Ok(());
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Ok(()),
        _ => Err(()),
    }
}

#[cfg(unix)]
fn kill_and_reap(child: &mut std::process::Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = kill_group(pid);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Non-unix fallback: reader thread, bounded wait for it after the kill.
#[cfg(not(unix))]
fn run_cli(mut cmd: Command, timeout: Duration) -> Result<Vec<u8>, String> {
    use std::io::Read;
    use std::sync::mpsc;

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to run secret CLI: {}", e.kind()))?;
    let stdout = child.stdout.take().ok_or("secret CLI stdout unavailable")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout
            .take(MAX_OUTPUT_BYTES as u64 + 1)
            .read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let deadline = deadline_after(timeout);
    let outcome: Result<std::process::ExitStatus, &str> = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    break Err("secret CLI timed out");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break Err("failed to wait for secret CLI"),
        }
    };
    if outcome.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let output = rx.recv_timeout(Duration::from_secs(2));
    match outcome {
        Err(msg) => Err(msg.to_string()),
        Ok(s) if !s.success() => Err("secret CLI exited with non-zero status".to_string()),
        Ok(_) => match output {
            Ok(b) if b.len() > MAX_OUTPUT_BYTES => Err(TOO_LARGE_REASON.to_string()),
            Ok(b) => Ok(b),
            Err(_) => Err("secret CLI output unavailable".to_string()),
        },
    }
}

impl SecretBackend for CliBackend {
    fn resolve(&self, reference: &str) -> Result<String, String> {
        self.resolve_within(reference, self.timeout)
    }

    fn resolve_within(&self, reference: &str, limit: Duration) -> Result<String, String> {
        let timeout = self.timeout.min(limit);
        let mut cmd;
        if let Some(rest) = reference.strip_prefix("keychain://") {
            let (service, account) = rest
                .split_once('/')
                .filter(|(s, a)| !s.is_empty() && !a.is_empty())
                .ok_or("malformed keychain reference (expected keychain://<service>/<account>)")?;
            cmd = Command::new("security");
            cmd.args(["find-generic-password", "-s", service, "-a", account, "-w"]);
        } else if let Some(rest) = reference.strip_prefix("op://") {
            if rest.split('/').filter(|p| !p.is_empty()).count() < 3 {
                return Err(
                    "malformed op reference (expected op://<vault>/<item>/<field>)".to_string(),
                );
            }
            cmd = Command::new("op");
            cmd.args(["read", reference]);
        } else {
            return Err("unsupported reference scheme".to_string());
        }
        let stdout = run_cli(cmd, timeout)?;
        let mut value = String::from_utf8(stdout)
            .map_err(|_| "secret CLI output is not valid UTF-8".to_string())?;
        if value.ends_with('\n') {
            value.pop();
            if value.ends_with('\r') {
                value.pop();
            }
        }
        if value.is_empty() {
            return Err("secret CLI returned an empty value".to_string());
        }
        Ok(value)
    }
}

/// Percent-encode every byte outside the RFC 3986 unreserved set.
fn percent_encode(s: &str, upper: bool, space_plus: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b' ' if space_plus => out.push('+'),
            _ if upper => out.push_str(&format!("%{b:02X}")),
            _ => out.push_str(&format!("%{b:02x}")),
        }
    }
    out
}

/// Forms produced by the `url` crate (used by reqwest) when the secret ends up
/// in a request URL: it percent-encodes only some characters per component
/// (`/`, `+`, `=` stay raw in a query), which differs from full percent
/// encoding. Derived from the library itself so they match exactly.
fn url_crate_variants(secret: &str) -> Vec<String> {
    let Ok(base) = reqwest::Url::parse("http://h/") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut u = base.clone();
    u.set_query(Some(secret));
    out.extend(u.query().map(str::to_string));
    let mut u = base.clone();
    u.set_path(secret);
    out.extend(u.path().strip_prefix('/').map(str::to_string));
    let mut u = base.clone();
    u.set_fragment(Some(secret));
    out.extend(u.fragment().map(str::to_string));
    let mut u = base.clone();
    u.query_pairs_mut().append_pair("k", secret);
    out.extend(
        u.query()
            .and_then(|q| q.strip_prefix("k="))
            .map(str::to_string),
    );
    // userinfo (`https://user:{{SECRET}}@host/`): different encode set again
    let mut u = base.clone();
    if u.set_username(secret).is_ok() {
        out.push(u.username().to_string());
    }
    let mut u = base;
    if u.set_password(Some(secret)).is_ok() {
        out.extend(u.password().map(str::to_string));
    }
    out
}

/// Textual forms a secret may take in outgoing data (besides the raw value):
/// URL/form percent-encoding and JSON string escaping.
fn secret_variants(secret: &str) -> Vec<String> {
    let mut v = vec![secret.to_string()];
    for (upper, plus) in [(true, false), (false, false), (true, true), (false, true)] {
        v.push(percent_encode(secret, upper, plus));
    }
    v.extend(url_crate_variants(secret));
    if let Ok(j) = serde_json::to_string(secret) {
        let inner = j[1..j.len() - 1].to_string();
        v.push(inner.replace('/', "\\/"));
        v.push(inner);
    }
    v
}

/// Masks secret values in outgoing data.
///
/// Covered forms: the exact value; full percent-encoding (upper/lower case hex,
/// with space as `%20` or `+`); the encodings the `url` crate (reqwest) applies
/// to a URL query, path, fragment, form-urlencoded pair value, and userinfo
/// (username / password); and the JSON-escaped string (with and without `\/`
/// escaping). Other transformed forms (base64, hex, ...) are NOT masked.
#[derive(Debug, Clone, Default)]
pub struct Masker {
    secrets: Vec<String>,
}

impl Masker {
    /// Build from secret values; values shorter than [`MIN_MASK_LEN`] are ignored.
    /// Longer forms are replaced first so overlapping secrets mask fully.
    pub fn new<I: IntoIterator<Item = String>>(values: I) -> Self {
        let mut secrets: Vec<String> = values
            .into_iter()
            .filter(|v| v.chars().count() >= MIN_MASK_LEN)
            .flat_map(|v| secret_variants(&v))
            .collect();
        secrets.sort();
        secrets.dedup();
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        Self { secrets }
    }

    pub fn mask_str(&self, input: &str) -> String {
        let mut out = input.to_string();
        for s in &self.secrets {
            if out.contains(s.as_str()) {
                out = out.replace(s.as_str(), MASK);
            }
        }
        out
    }

    /// Mask header map: `Authorization` always, other values by substring.
    /// Header names are kept as-is (masking them could collide two keys).
    pub fn mask_headers(&self, headers: &HashMap<String, String>) -> HashMap<String, String> {
        headers
            .iter()
            .map(|(k, v)| {
                let nv = if k.eq_ignore_ascii_case("authorization") {
                    MASK.to_string()
                } else {
                    self.mask_str(v)
                };
                (k.clone(), nv)
            })
            .collect()
    }

    pub fn mask_result(&self, r: &mut TestResult) {
        r.name = self.mask_str(&r.name);
        r.response_preview = self.mask_str(&r.response_preview);
        r.request_url = self.mask_str(&r.request_url);
        r.request_headers = self.mask_headers(&r.request_headers);
        r.request_body = r.request_body.as_deref().map(|b| self.mask_str(b));
        r.response_headers = self.mask_headers(&r.response_headers);
        for f in &mut r.failures {
            f.expected = self.mask_str(&f.expected);
            f.actual = self.mask_str(&f.actual);
        }
    }

    /// Mask a whole suite result in place (call before any rendering).
    pub fn mask_suite(&self, s: &mut SuiteResult) {
        for r in &mut s.results {
            self.mask_result(r);
        }
        for w in &mut s.warnings {
            *w = self.mask_str(w);
        }
    }

    pub fn mask_response(&self, r: &mut HttpResponse) {
        r.headers = self.mask_headers(&r.headers);
        r.actual_request_headers = self.mask_headers(&r.actual_request_headers);
        r.body = self.mask_str(&r.body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Failure;

    fn result_with(secret: &str) -> TestResult {
        TestResult {
            name: "t".into(),
            status: 200,
            passed: false,
            elapsed_ms: 1,
            failures: vec![Failure {
                check: "body".into(),
                expected: "x".into(),
                actual: format!("got {secret}"),
            }],
            response_preview: format!("{{\"echo\":\"{secret}\"}}"),
            request_method: "GET".into(),
            request_url: format!("https://h/?k={secret}"),
            request_headers: HashMap::from([
                ("authorization".to_string(), format!("Bearer {secret}")),
                ("X-Other".to_string(), format!("v-{secret}")),
            ]),
            request_body: Some(format!("body {secret}")),
            response_headers: HashMap::new(),
            response_http_version: String::new(),
            response_charset: None,
            response_size_bytes: 0,
            skipped: false,
            error_type: None,
        }
    }

    #[test]
    fn masks_everywhere_in_result() {
        let secret = "s3cr3t-value-123";
        let m = Masker::new([secret.to_string()]);
        let mut r = result_with(secret);
        m.mask_result(&mut r);
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains(secret), "{json}");
        assert_eq!(r.request_headers["authorization"], MASK);
        assert_eq!(r.request_headers["X-Other"], "v-***");
    }

    #[test]
    fn authorization_masked_even_without_secrets() {
        let m = Masker::default();
        let h = HashMap::from([("AUTHORIZATION".to_string(), "Bearer short".to_string())]);
        assert_eq!(m.mask_headers(&h)["AUTHORIZATION"], MASK);
    }

    #[test]
    fn min_length_rule() {
        let m = Masker::new(["1234567".to_string(), "12345678".to_string()]);
        assert_eq!(m.mask_str("a 1234567 b"), "a 1234567 b");
        assert_eq!(m.mask_str("a 12345678 b"), "a *** b");
    }

    #[test]
    fn mask_suite_covers_warnings() {
        let secret = "warn-secret-9999";
        let m = Masker::new([secret.to_string()]);
        let mut s = SuiteResult {
            total: 1,
            passed: 0,
            failed: 1,
            skipped: 0,
            elapsed_ms: 0,
            results: vec![result_with(secret)],
            warnings: vec![format!("last error {secret}")],
        };
        m.mask_suite(&mut s);
        assert!(!serde_json::to_string(&s).unwrap().contains(secret));
    }

    #[test]
    fn reference_detection() {
        assert!(is_reference("keychain://a/b"));
        assert!(is_reference("op://v/i/f"));
        assert!(!is_reference("https://x"));
        assert!(!is_reference(" keychain://a/b"));
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("keychain://a/b", "keychain://a/b"));
        assert!(!glob_match("keychain://a/b", "keychain://a/bc"));
        assert!(glob_match(
            "keychain://my-api-tests/*",
            "keychain://my-api-tests/API_KEY"
        ));
        assert!(glob_match(
            "keychain://my-api-tests/*",
            "keychain://my-api-tests/x/y/z"
        ));
        assert!(!glob_match(
            "keychain://my-api-tests/*",
            "keychain://other/API_KEY"
        ));
        assert!(!glob_match(
            "keychain://my-api-tests/*",
            "keychain://my-api-tests"
        ));
        assert!(glob_match("op://Dev/*", "op://Dev/item/field"));
        assert!(!glob_match("op://Dev/*", "op://Prod/item/field"));
        assert!(glob_match("op://*/item/*", "op://Dev/item/field"));
        assert!(!glob_match("op://*/item/*", "op://Dev/other/field"));
        assert!(glob_match("*", "anything://at/all"));
        assert!(glob_match("a*b*c", "aXbYc"));
        assert!(glob_match("a*b*c", "abc"));
        assert!(!glob_match("a*b*c", "acb"));
        assert!(!glob_match("a*bc", "abc_"));
        // prefix and suffix must not overlap
        assert!(!glob_match("ab*ba", "aba"));
        assert!(glob_match("ab*ba", "abba"));
    }

    #[test]
    fn allowlist_semantics() {
        assert!(!SecretAllowlist::default().is_allowed("keychain://a/b"));
        let al = SecretAllowlist::new(["keychain://svc/*".to_string(), "op://V/I/F".to_string()]);
        assert!(al.is_allowed("keychain://svc/acct"));
        assert!(al.is_allowed("op://V/I/F"));
        assert!(!al.is_allowed("op://V/I/G"));
        assert!(!al.is_allowed("keychain://evil/acct"));
    }

    #[test]
    fn masks_percent_encoded_and_json_escaped_forms() {
        let secret = "p@ss w/rd\\\"q&=+x9";
        let m = Masker::new([secret.to_string()]);
        let pct = percent_encode(secret, true, false);
        let pct_lower = percent_encode(secret, false, false);
        let form = percent_encode(secret, true, true);
        let json = serde_json::to_string(secret).unwrap();
        let json_inner = &json[1..json.len() - 1];
        for form in [secret, &pct, &pct_lower, &form.clone(), json_inner] {
            let out = m.mask_str(&format!("a={form}&b"));
            assert_eq!(out, "a=***&b", "form {form}");
        }
        // base64 is documented as NOT masked
        assert_eq!(m.mask_str("cEBzcw=="), "cEBzcw==");
    }

    #[test]
    fn short_secret_variants_not_masked() {
        let m = Masker::new(["a b".to_string()]);
        assert_eq!(m.mask_str("a%20b a+b a b"), "a%20b a+b a b");
    }

    /// Kills exactly the PID a test shell recorded in a side-channel file,
    /// also when an assertion panics. No pattern-based process killing.
    #[cfg(unix)]
    struct PidGuard(std::path::PathBuf);

    #[cfg(unix)]
    impl PidGuard {
        fn new(tag: &str) -> Self {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!(
                "rest-e2e-mcp-test-{}-{tag}-{n}.pid",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&p);
            PidGuard(p)
        }
        fn path(&self) -> String {
            self.0.display().to_string()
        }
    }

    #[cfg(unix)]
    impl Drop for PidGuard {
        fn drop(&mut self) {
            // The recording shell may still be starting: wait briefly for a pid.
            for _ in 0..50 {
                if let Ok(text) = std::fs::read_to_string(&self.0)
                    && let Ok(pid) = text.trim().parse::<i32>()
                    && pid > 1
                {
                    // SAFETY: plain kill of the single pid we recorded.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_times_out_and_kills() {
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let t = Instant::now();
        let err = run_cli(cmd, Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_success_with_backgrounded_grandchild_returns_promptly() {
        let g = PidGuard::new("bg");
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!("echo ok; sleep 30 & echo $! > '{}'", g.path()),
        ]);
        let t = Instant::now();
        let out = run_cli(cmd, Duration::from_secs(5)).unwrap();
        assert_eq!(out, b"ok\n");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_timeout_kills_group_with_grandchild() {
        let g = PidGuard::new("grp");
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!("sleep 30 & echo $! > '{}'; sleep 30", g.path()),
        ]);
        let t = Instant::now();
        let err = run_cli(cmd, Duration::from_millis(200)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_slow_success_is_not_reported_as_timeout() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 0.2; echo late"]);
        assert_eq!(run_cli(cmd, Duration::from_secs(5)).unwrap(), b"late\n");
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_returns_stdout() {
        let mut cmd = Command::new("echo");
        cmd.arg("hi");
        assert_eq!(run_cli(cmd, Duration::from_secs(5)).unwrap(), b"hi\n");
    }

    #[test]
    fn masks_userinfo_encoded_forms() {
        let secret = "ab/c+d@e:f#g?h%i";
        let m = Masker::new([secret.to_string()]);
        let mut u = reqwest::Url::parse("https://h/").unwrap();
        u.set_username("u").unwrap();
        u.set_password(Some(secret)).unwrap();
        assert!(!u.as_str().contains(secret), "{u}");
        assert_eq!(m.mask_str(u.as_str()), "https://u:***@h/");
        let mut u = reqwest::Url::parse("https://h/").unwrap();
        u.set_username(secret).unwrap();
        assert_eq!(m.mask_str(u.as_str()), "https://***@h/");
    }

    #[test]
    fn deadline_after_saturates_instead_of_panicking() {
        let d = deadline_after(Duration::MAX);
        assert!(d > Instant::now() + Duration::from_secs(24 * 3600));
        let _ = deadline_after(Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_rejects_oversized_output() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "head -c 200000 /dev/zero | tr '\\0' a"]);
        let t = Instant::now();
        let err = run_cli(cmd, Duration::from_secs(5)).unwrap_err();
        assert_eq!(err, "secret CLI output too large");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_accepts_output_at_cap() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "head -c 65536 /dev/zero | tr '\\0' a"]);
        let out = run_cli(cmd, Duration::from_secs(5)).unwrap();
        assert_eq!(out.len(), MAX_OUTPUT_BYTES);
    }

    /// A descendant that escapes the process group (setsid) and keeps the
    /// stdout pipe open must not block or outlive `run_cli`.
    #[cfg(unix)]
    #[test]
    fn run_cli_setsid_grandchild_holding_pipe_returns_bounded() {
        // There is no `setsid` command on macOS, so the escapee calls
        // POSIX::setsid() from perl (present on Linux and macOS). It writes its
        // pid to the guard file, then `exec`s sleep, keeping the same pid.
        // Paths go in as arguments, so no shell quoting is involved.
        const ESCAPE: &str = "perl -MPOSIX -e 'POSIX::setsid() or die; \
             open(my $f, \">\", $ARGV[0]) or die; print $f $$; close $f; \
             exec \"sleep\", $ARGV[1]' \"$1\" \"$2\"";
        assert!(
            Command::new("perl").arg("-v").output().is_ok(),
            "perl is required for this test"
        );
        // The recorded pid must be a session leader, i.e. it really left the group.
        fn assert_escaped(g: &PidGuard) {
            let t = Instant::now();
            let pid = loop {
                if let Ok(s) = std::fs::read_to_string(&g.0)
                    && let Ok(p) = s.trim().parse::<i32>()
                {
                    break p;
                }
                assert!(
                    t.elapsed() < Duration::from_secs(3),
                    "escapee never started"
                );
                std::thread::sleep(Duration::from_millis(20));
            };
            // SAFETY: getsid only queries the given pid.
            assert_eq!(unsafe { libc::getsid(pid) }, pid, "escapee did not setsid");
        }

        let g1 = PidGuard::new("setsid1");
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!("echo ok; {ESCAPE} & exit 0"),
            "sh",
            &g1.path(),
            "31",
        ]);
        let t = Instant::now();
        let out = run_cli(cmd, Duration::from_secs(5)).unwrap();
        assert_eq!(out, b"ok\n");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert_escaped(&g1);

        // timeout path with the same escapee
        let g2 = PidGuard::new("setsid2");
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!("{ESCAPE} & sleep 31"),
            "sh",
            &g2.path(),
            "32",
        ]);
        let t = Instant::now();
        let err = run_cli(cmd, Duration::from_millis(1000)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert_escaped(&g2);
        // the escaped sleeps are outside the group on purpose; guards kill them
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_success_leaves_same_group_helper_alive() {
        // child exits at once; a same-group helper keeps the pipe and must
        // NOT be killed on success.
        let g = PidGuard::new("helper");
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!("echo done; sleep 30 & echo $! > '{}'; exit 0", g.path()),
        ]);
        let t = Instant::now();
        assert_eq!(run_cli(cmd, Duration::from_secs(5)).unwrap(), b"done\n");
        assert!(t.elapsed() < Duration::from_secs(3));
        let pid: i32 = std::fs::read_to_string(&g.0)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only probes for existence.
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "helper was killed");
    }

    #[cfg(unix)]
    #[test]
    fn run_cli_keeps_all_output_of_child_that_wrote_then_exited() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "head -c 32768 /dev/zero | tr '\\0' a; exit 0"]);
        let out = run_cli(cmd, Duration::from_secs(5)).unwrap();
        assert_eq!(out.len(), 32768);
        // and with a pipe-holding grandchild around
        let g = PidGuard::new("big");
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            &format!(
                "sleep 30 & echo $! > '{}'; head -c 32768 /dev/zero | tr '\\0' b; exit 0",
                g.path()
            ),
        ]);
        let t = Instant::now();
        let out = run_cli(cmd, Duration::from_secs(5)).unwrap();
        assert_eq!(out.len(), 32768);
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn masks_url_crate_encoded_forms() {
        let secret = "ab/c+d=e f\"g#h";
        let m = Masker::new([secret.to_string()]);
        let base = reqwest::Url::parse("http://h/").unwrap();
        // query as the url crate encodes it (raw `/`, `+`, `=`)
        let mut u = base.clone();
        u.set_query(Some(secret));
        let q = u.query().unwrap().to_string();
        assert!(q.contains("ab/c+d=e"), "{q}");
        assert_eq!(m.mask_str(&format!("http://h/?k={q}")), "http://h/?k=***");
        // form-urlencoded pair value
        let mut u = base.clone();
        u.query_pairs_mut().append_pair("k", secret);
        let form = u.query().unwrap().strip_prefix("k=").unwrap().to_string();
        assert_eq!(m.mask_str(&format!("x?k={form}")), "x?k=***");
        // path form
        let mut u = base;
        u.set_path(secret);
        let p = u.path().trim_start_matches('/').to_string();
        assert_eq!(m.mask_str(&format!("http://h/{p}")), "http://h/***");
        // end-to-end: a URL built by reqwest from a string with the secret
        let built = reqwest::Url::parse(&format!("http://h/p?k={secret}")).unwrap();
        assert_eq!(m.mask_str(built.as_str()), "http://h/p?k=***");
    }

    #[test]
    fn mask_headers_keeps_names_and_does_not_collide() {
        let m = Masker::new(["key-secret-1".to_string(), "key-secret-2".to_string()]);
        let h = HashMap::from([
            ("key-secret-1".to_string(), "a".to_string()),
            ("key-secret-2".to_string(), "b".to_string()),
        ]);
        let out = m.mask_headers(&h);
        assert_eq!(out.len(), 2);
        assert_eq!(out["key-secret-1"], "a");
    }

    #[test]
    fn allowlist_report_warns_on_broad_patterns() {
        let r = allowlist_report(&["*".to_string()]);
        assert!(
            r.iter()
                .any(|l| l.starts_with("WARNING") && l.contains("EVERY"))
        );
        let r = allowlist_report(&["svc/*".to_string()]);
        assert!(
            r.iter()
                .any(|l| l.starts_with("WARNING") && l.contains("known scheme"))
        );
        let r = allowlist_report(&["keychain://svc/*".to_string(), "op://V/*".to_string()]);
        assert!(!r.iter().any(|l| l.starts_with("WARNING")));
        assert!(r.iter().any(|l| l.contains("allow: op://V/*")));
        let r = allowlist_report(&[]);
        assert!(r[0].contains("empty"));
    }

    #[test]
    fn cli_backend_rejects_malformed() {
        assert!(
            CliBackend::default()
                .resolve("keychain://onlyservice")
                .is_err()
        );
        assert!(CliBackend::default().resolve("keychain:///acct").is_err());
        assert!(CliBackend::default().resolve("op://vault/item").is_err());
        assert!(CliBackend::default().resolve("other://x").is_err());
    }
}
