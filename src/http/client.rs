use std::collections::HashMap;
use std::time::Instant;

use crate::types::HttpResponse;

/// HTTPリクエスト実行時のエラー種別。
#[derive(Debug, Clone, thiserror::Error)]
pub enum RequestError {
    /// タイムアウト（制限時間超過）。
    #[error("Timeout after {elapsed_ms}ms (limit: {limit_ms}ms)")]
    Timeout { elapsed_ms: u64, limit_ms: u64 },
    /// TCP接続エラー（拒否、リセット等）。
    #[error("Connection error: {0}")]
    Connection(String),
    /// DNS解決失敗。
    #[error("DNS resolution failed: {0}")]
    Dns(String),
    /// TLS/SSLエラー。
    #[error("TLS error: {0}")]
    Tls(String),
    /// その他のエラー。
    #[error("Request error: {0}")]
    Other(String),
}

impl RequestError {
    /// エラー種別を短縮キーとして返す（circuit breaker 用）。
    pub fn error_key(&self) -> &'static str {
        match self {
            Self::Timeout { .. } => "timeout",
            Self::Connection(_) => "connection",
            Self::Dns(_) => "dns",
            Self::Tls(_) => "tls",
            Self::Other(_) => "other",
        }
    }

    /// reqwest::Error を分類して RequestError に変換する。
    fn classify(err: reqwest::Error, elapsed_ms: u64, limit_ms: u64) -> Self {
        if err.is_timeout() {
            return Self::Timeout {
                elapsed_ms,
                limit_ms,
            };
        }
        if err.is_connect() {
            return Self::Connection(err.without_url().to_string());
        }
        let msg_lower = err.to_string().to_lowercase();
        if msg_lower.contains("dns") || msg_lower.contains("resolve") {
            return Self::Dns(err.without_url().to_string());
        }
        if msg_lower.contains("tls")
            || msg_lower.contains("ssl")
            || msg_lower.contains("certificate")
        {
            return Self::Tls(err.without_url().to_string());
        }
        Self::Other(err.without_url().to_string())
    }
}

/// 共有HTTPクライアントを構築する。スイート内で再利用してコネクションプールを活用する。
pub fn build_client() -> Result<reqwest::Client, RequestError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| RequestError::Other(format!("Failed to build HTTP client: {e}")))
}

/// HTTPリクエストを実行する。
pub async fn execute_request(
    client: &reqwest::Client,
    method: &str,
    url: &str,
    headers: &HashMap<String, String>,
    body: Option<&str>,
    timeout_ms: u64,
) -> Result<HttpResponse, RequestError> {
    let method = method
        .parse::<reqwest::Method>()
        .map_err(|e| RequestError::Other(format!("Invalid HTTP method: {e}")))?;

    let mut req = client.request(method, url);

    for (key, val) in headers {
        req = req.header(key.as_str(), val.as_str());
    }

    if let Some(body) = body {
        req = req.body(body.to_string());
    }

    // build() で実際のリクエストオブジェクトを生成し、送信ヘッダーを取得
    let built = req
        .build()
        .map_err(|e| RequestError::Other(format!("Failed to build request: {e}")))?;

    let mut actual_request_headers: HashMap<String, String> = HashMap::new();
    for (key, val) in built.headers() {
        if let Ok(v) = val.to_str() {
            actual_request_headers.insert(key.as_str().to_string(), v.to_string());
        }
    }

    let start = Instant::now();
    let timeout_duration = std::time::Duration::from_millis(timeout_ms);

    let response = tokio::time::timeout(timeout_duration, client.execute(built))
        .await
        .map_err(|_| RequestError::Timeout {
            elapsed_ms: elapsed_ms_saturating(&start),
            limit_ms: timeout_ms,
        })?
        .map_err(|e| RequestError::classify(e, elapsed_ms_saturating(&start), timeout_ms))?;

    let elapsed_ms = elapsed_ms_saturating(&start);

    let status = response.status().as_u16();
    let http_version = match response.version() {
        reqwest::Version::HTTP_09 => "HTTP/0.9",
        reqwest::Version::HTTP_10 => "HTTP/1.0",
        reqwest::Version::HTTP_11 => "HTTP/1.1",
        reqwest::Version::HTTP_2 => "HTTP/2",
        reqwest::Version::HTTP_3 => "HTTP/3",
        _ => "HTTP/?",
    }
    .to_string();

    // レスポンスヘッダー収集
    let mut resp_headers: HashMap<String, String> = HashMap::new();
    for (key, val) in response.headers() {
        if let Ok(v) = val.to_str() {
            resp_headers.insert(key.as_str().to_string(), v.to_string());
        }
    }

    // charset検出
    let charset = detect_charset(&resp_headers);

    // ボディ取得・エンコーディング変換
    let bytes = response
        .bytes()
        .await
        .map_err(|e| RequestError::Other(format!("Failed to read response body: {e}")))?;
    let size_bytes = bytes.len();

    let body = decode_body(&bytes, charset.as_deref());

    Ok(HttpResponse {
        status,
        http_version,
        headers: resp_headers,
        actual_request_headers,
        body,
        charset,
        elapsed_ms,
        size_bytes,
    })
}

/// `Instant::elapsed()` を `u64` ミリ秒に安全変換する。
fn elapsed_ms_saturating(start: &Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}

/// Content-Typeヘッダーからcharsetを検出する。
fn detect_charset(headers: &HashMap<String, String>) -> Option<String> {
    let ct = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v)?;

    ct.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix("charset=")
            .map(|charset| charset.trim_matches('"').to_lowercase())
    })
}

/// バイト列を指定charsetでUTF-8に変換する。
fn decode_body(bytes: &[u8], charset: Option<&str>) -> String {
    let charset = charset.unwrap_or("utf-8");

    match charset {
        "utf-8" | "utf8" => String::from_utf8_lossy(bytes).into_owned(),
        _ => {
            let encoding =
                encoding_rs::Encoding::for_label(charset.as_bytes()).unwrap_or(encoding_rs::UTF_8);
            let (decoded, _, _) = encoding.decode(bytes);
            decoded.into_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_charset_from_content_type() {
        let mut headers = HashMap::new();
        headers.insert(
            "content-type".to_string(),
            "text/csv; charset=Shift_JIS".to_string(),
        );
        assert_eq!(detect_charset(&headers), Some("shift_jis".to_string()));
    }

    #[test]
    fn detect_charset_missing() {
        let headers = HashMap::new();
        assert_eq!(detect_charset(&headers), None);
    }

    #[test]
    fn detect_charset_no_charset_param() {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        assert_eq!(detect_charset(&headers), None);
    }

    #[test]
    fn decode_utf8() {
        let bytes = "こんにちは".as_bytes();
        assert_eq!(decode_body(bytes, Some("utf-8")), "こんにちは");
    }

    #[test]
    fn decode_shift_jis() {
        let (encoded, _, _) = encoding_rs::SHIFT_JIS.encode("テスト");
        let decoded = decode_body(&encoded, Some("shift_jis"));
        assert_eq!(decoded, "テスト");
    }

    #[test]
    fn request_error_timeout_display() {
        let err = RequestError::Timeout {
            elapsed_ms: 30019,
            limit_ms: 30000,
        };
        assert_eq!(err.to_string(), "Timeout after 30019ms (limit: 30000ms)");
        assert_eq!(err.error_key(), "timeout");
    }

    #[test]
    fn request_error_connection_display() {
        let err = RequestError::Connection("connection refused".to_string());
        assert_eq!(err.to_string(), "Connection error: connection refused");
        assert_eq!(err.error_key(), "connection");
    }

    #[test]
    fn request_error_dns_display() {
        let err = RequestError::Dns("failed to lookup address".to_string());
        assert_eq!(
            err.to_string(),
            "DNS resolution failed: failed to lookup address"
        );
        assert_eq!(err.error_key(), "dns");
    }

    #[test]
    fn request_error_tls_display() {
        let err = RequestError::Tls("certificate verify failed".to_string());
        assert_eq!(err.to_string(), "TLS error: certificate verify failed");
        assert_eq!(err.error_key(), "tls");
    }

    #[test]
    fn request_error_other_display() {
        let err = RequestError::Other("something went wrong".to_string());
        assert_eq!(err.to_string(), "Request error: something went wrong");
        assert_eq!(err.error_key(), "other");
    }

    #[test]
    fn elapsed_ms_saturating_normal() {
        let start = Instant::now();
        let ms = elapsed_ms_saturating(&start);
        assert!(ms < 1000);
    }
}
