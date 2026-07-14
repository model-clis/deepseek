use anyhow::Result;
use reqwest::{StatusCode, header::RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{error::Error, fmt, time::Duration};

#[derive(Debug)]
pub enum ApiError {
    ContextLength(String),
    Http(StatusCode, String),
    Transport(String),
    InvalidResponse(String),
}
impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ContextLength(body) => write!(f, "API context length exceeded: {body}"),
            Self::Http(s, b) => write!(f, "API {s}: {b}"),
            Self::Transport(e) => write!(f, "API transport error: {e}"),
            Self::InvalidResponse(e) => write!(f, "Invalid API response: {e}"),
        }
    }
}
impl Error for ApiError {}

pub const DEFAULT_BASE: &str = "https://api.deepseek.com";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}
#[derive(Debug, Deserialize)]
pub struct Response {
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}
#[derive(Debug, Deserialize)]
pub struct Choice {
    pub message: Message,
    pub finish_reason: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'static str,
    messages: &'a [Message],
    tools: &'a [Value],
    stream: bool,
    thinking: Value,
    reasoning_effort: &'static str,
    max_tokens: u32,
    tool_choice: &'static str,
}
pub struct Client {
    http: reqwest::Client,
    key: String,
    base: String,
}
impl Client {
    pub fn new(key: String, base: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()?,
            key,
            base: base.trim_end_matches('/').into(),
        })
    }
    pub async fn verify(&self) -> Result<()> {
        let r = self
            .http
            .get(format!("{}/models", self.base))
            .bearer_auth(&self.key)
            .send()
            .await?;
        if !r.status().is_success() {
            return Err(ApiError::Http(r.status(), r.text().await.unwrap_or_default()).into());
        }
        Ok(())
    }
    pub async fn chat(&self, messages: &[Message], tools: &[Value]) -> Result<Response> {
        let body = serde_json::to_vec(&Request {
            model: "deepseek-v4-flash",
            messages,
            tools,
            stream: false,
            thinking: serde_json::json!({"type":"enabled"}),
            reasoning_effort: "max",
            max_tokens: 131072,
            tool_choice: "auto",
        })?;
        for attempt in 0..=3 {
            let sent = self
                .http
                .post(format!("{}/chat/completions", self.base))
                .bearer_auth(&self.key)
                .header("content-type", "application/json")
                .body(body.clone())
                .send()
                .await;
            match sent {
                Ok(r) if r.status().is_success() => {
                    let bytes = r
                        .bytes()
                        .await
                        .map_err(|e| ApiError::Transport(e.to_string()))?;
                    return serde_json::from_slice(&bytes)
                        .map_err(|e| ApiError::InvalidResponse(e.to_string()).into());
                }
                Ok(r) => {
                    let status = r.status();
                    let retry_after = r
                        .headers()
                        .get(RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_retry_after);
                    let text = r.text().await.unwrap_or_default();
                    if status == StatusCode::BAD_REQUEST && is_context_error(&text) {
                        return Err(ApiError::ContextLength(text).into());
                    }
                    if !retryable_status(status) || attempt == 3 {
                        return Err(ApiError::Http(status, text).into());
                    }
                    eprintln!("API retry {}/3", attempt + 1);
                    tokio::time::sleep(Duration::from_secs(retry_after.unwrap_or(1 << attempt)))
                        .await;
                }
                Err(e) => {
                    if attempt == 3 {
                        return Err(ApiError::Transport(e.to_string()).into());
                    }
                    eprintln!("Network retry {}/3", attempt + 1);
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                }
            }
        }
        unreachable!()
    }
}
fn is_context_error(body: &str) -> bool {
    let s = body.to_ascii_lowercase();
    (s.contains("context_length_exceeded")
        || s.contains("context length")
        || s.contains("maximum context")
        || s.contains("max context")
        || s.contains("input is too long")
        || s.contains("request too large")
        || s.contains("too many tokens")
        || s.contains("maximum number of tokens"))
        || (s.contains("context")
            && (s.contains("token limit") || s.contains("too long") || s.contains("exceed")))
}
fn parse_retry_after(value: &str) -> Option<u64> {
    value.parse().ok().or_else(|| {
        httpdate::parse_http_date(value).ok().map(|t| {
            t.duration_since(std::time::SystemTime::now())
                .unwrap_or_default()
                .as_secs()
        })
    })
}
pub fn retryable_status(s: StatusCode) -> bool {
    matches!(s.as_u16(), 429 | 500 | 502 | 503 | 504)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use wiremock::{
        Mock, MockServer, Request, ResponseTemplate,
        matchers::{method, path},
    };

    #[test]
    fn context_variants_and_retry_status() {
        for body in [
            "context_length_exceeded",
            "Maximum context size reached",
            "input is too long",
            "too many tokens",
        ] {
            assert!(is_context_error(body), "{body}");
        }
        assert!(!is_context_error("ordinary bad request"));
        assert!(retryable_status(StatusCode::TOO_MANY_REQUESTS));
    }

    #[tokio::test]
    async fn retry_reuses_identical_body_and_invalid_success_is_typed() {
        let server = MockServer::start().await;
        let bodies = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let seen = bodies.clone();
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(move |r: &Request| {
                let mut all = seen.lock().unwrap();
                all.push(r.body.clone());
                if all.len() == 1 {
                    ResponseTemplate::new(429).insert_header("Retry-After", "0")
                } else {
                    ResponseTemplate::new(200).set_body_string("not json")
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        let client = Client::new("key".into(), &server.uri()).unwrap();
        let error = client.chat(&[], &[]).await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ApiError>(),
            Some(ApiError::InvalidResponse(_))
        ));
        let all = bodies.lock().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], all[1]);
    }
}
