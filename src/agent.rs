use crate::{
    api::{ApiError, Client, Message},
    tools,
};
use anyhow::{Result, bail};
use std::path::PathBuf;

const CONTEXT_GUARD: u64 = 900_000;

pub struct Outcome {
    pub exit_code: u8,
    pub report: Option<String>,
}

fn msg(role: &str, content: impl Into<String>) -> Message {
    Message {
        role: role.into(),
        content: Some(content.into()),
        reasoning_content: None,
        tool_calls: None,
        tool_call_id: None,
    }
}

/// Startup metadata is constructed once by `run` and then lives in history unchanged.
fn system(cwd: &std::path::Path, shell: &tools::ShellInfo) -> String {
    let os = os_info::get();
    let now = chrono::Local::now();
    let timezone = iana_time_zone::get_timezone().unwrap_or_else(|_| "unknown".into());
    format!(
        "You are a general-purpose agent. Follow the user's scope, side-effect, and output-format constraints exactly; plan before acting and use only the tool calls needed. Treat tool results as authoritative: check ok, command_succeeded, exit_code, timed_out, truncated, and next_offset before claiming success or completeness. Environment: OS={} {}; arch={}; shell={}. The startup cwd is {}; relative paths resolve from it and absolute paths remain absolute. The local startup datetime is {} ({}). CLI version: {}. Tools: read reads files, write creates or replaces files, edit performs a unique replacement, and shell runs commands.",
        os.os_type(),
        os.version(),
        std::env::consts::ARCH,
        shell.description,
        cwd.display(),
        now.to_rfc3339(),
        timezone,
        env!("CARGO_PKG_VERSION")
    )
}

fn serialized_tokens(messages: &[Message]) -> u64 {
    // Deliberately conservative and deterministic; includes reasoning, tool arguments and results.
    if messages.is_empty() {
        return 0;
    }
    serde_json::to_vec(messages)
        .map(|v| v.len().div_ceil(3) as u64)
        .unwrap_or(u64::MAX)
}

fn projected_tokens(usage: Option<u64>, history: &[Message], sent_len: usize) -> u64 {
    match usage {
        Some(prompt_tokens) => {
            prompt_tokens.saturating_add(serialized_tokens(&history[sent_len..]))
        }
        None => serialized_tokens(history),
    }
}

fn context_outcome(detail: &str) -> Outcome {
    Outcome {
        exit_code: 2,
        report: Some(format!("## Incomplete\n\n{detail}")),
    }
}

fn is_context(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>()
        .is_some_and(|e| matches!(e, ApiError::ContextLength(_)))
}

pub async fn run(client: Client, prompt: String, cwd: PathBuf, max_turns: u32) -> Result<Outcome> {
    let defs = tools::definitions();
    let shell_info = tools::ShellInfo::detect().map_err(anyhow::Error::msg)?;
    let mut history = vec![
        msg("system", system(&cwd, &shell_info)),
        msg("user", prompt),
    ];
    let mut fragments = String::new();

    for work_call in 1..=max_turns {
        eprintln!("Work call {work_call}/{max_turns}");
        let sent_len = history.len();
        let response = match client.chat(&history, &defs).await {
            Ok(response) => response,
            Err(e) if is_context(&e) => {
                return Ok(context_outcome("API context length exceeded during work."));
            }
            Err(e) => return Err(e),
        };
        let prompt_tokens = response.usage.as_ref().map(|u| u.prompt_tokens);
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::InvalidResponse("missing choice 0".into()))?;
        let calls = choice.message.tool_calls.clone().unwrap_or_default();
        let finish_reason = choice.finish_reason.as_deref();

        if !calls.is_empty() {
            if !matches!(finish_reason, Some("tool_calls") | Some("stop")) {
                bail!("Invalid finish_reason for tool calls: {finish_reason:?}");
            }
            fragments.clear();
            history.push(choice.message);
            for call in calls {
                eprintln!("Tool started: {}", call.function.name);
                let result = tools::execute(
                    &call.function.name,
                    &call.function.arguments,
                    &cwd,
                    &shell_info,
                )
                .await;
                eprintln!("Tool finished: {}", call.function.name);
                history.push(Message {
                    role: "tool".into(),
                    content: Some(result),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: Some(call.id),
                });
            }
        } else {
            match finish_reason {
                Some("stop") => {
                    let content = choice.message.content.clone().unwrap_or_default();
                    history.push(choice.message);
                    if content.is_empty() {
                        bail!("Final response was empty");
                    }
                    fragments.push_str(&content);
                    return Ok(Outcome {
                        exit_code: 0,
                        report: Some(fragments),
                    });
                }
                Some("length") => {
                    fragments.push_str(choice.message.content.as_deref().unwrap_or_default());
                    history.push(choice.message);
                    history.push(msg(
                        "user",
                        "Continue from the interrupted position without repeating.",
                    ));
                }
                Some("content_filter") => bail!("Response was content-filtered"),
                Some("insufficient_system_resource") => bail!(
                    "Server resources were insufficient; not retrying to avoid duplicate effects"
                ),
                Some(other) => bail!("Invalid finish_reason: {other:?}"),
                None => bail!("API response omitted finish_reason"),
            }
        }

        let why = if work_call == max_turns {
            Some("max turns reached")
        } else if projected_tokens(prompt_tokens, &history, sent_len) >= CONTEXT_GUARD {
            Some("context budget threshold reached")
        } else {
            None
        };
        if let Some(why) = why {
            return finish(client, &defs, &mut history, why, &mut fragments).await;
        }
    }

    finish(
        client,
        &defs,
        &mut history,
        "max turns reached",
        &mut fragments,
    )
    .await
}

async fn finish(
    client: Client,
    defs: &[serde_json::Value],
    history: &mut Vec<Message>,
    why: &str,
    fragments: &mut String,
) -> Result<Outcome> {
    fragments.clear();
    history.push(msg("user", format!("The work phase ended ({why}). Do not use tools. Output a clear Markdown incomplete report describing what was and was not completed, without suggestions.")));
    for _ in 0..3 {
        let response = match client.chat(history, defs).await {
            Ok(response) => response,
            Err(e) if is_context(&e) => {
                return Ok(context_outcome(
                    "API context length exceeded while producing the incomplete report.",
                ));
            }
            Err(e) => return Err(e),
        };
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::InvalidResponse("missing choice 0".into()))?;
        let calls = choice.message.tool_calls.clone().unwrap_or_default();
        let reason = choice.finish_reason.as_deref();
        if !calls.is_empty() {
            if !matches!(reason, Some("tool_calls") | Some("stop")) {
                bail!("Invalid finish_reason for finish tool calls: {reason:?}");
            }
            fragments.clear();
            history.push(choice.message);
            for call in calls {
                history.push(Message {
                    role: "tool".into(),
                    content: Some(r#"{"ok":false,"error":"Tools are disabled because the work phase ended; output the incomplete report"}"#.into()),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: Some(call.id),
                });
            }
            continue;
        }
        match reason {
            Some("length") => {
                fragments.push_str(choice.message.content.as_deref().unwrap_or_default());
                history.push(choice.message);
                history.push(msg("user", "Continue the incomplete report from the interrupted position without repeating."));
            }
            Some("stop") => {
                let text = choice.message.content.clone().unwrap_or_default();
                history.push(choice.message);
                if text.is_empty() {
                    history.push(msg(
                        "user",
                        "The report was empty. Output the Markdown incomplete report now.",
                    ));
                } else {
                    fragments.push_str(&text);
                    return Ok(Outcome {
                        exit_code: 2,
                        report: Some(fragments.clone()),
                    });
                }
            }
            Some("content_filter") => bail!("Finish response was content-filtered"),
            Some("insufficient_system_resource") => {
                bail!("Server resources were insufficient while producing finish report")
            }
            Some(other) => bail!("Invalid finish_reason: {other:?}"),
            None => bail!("Finish response omitted finish_reason"),
        }
    }
    Ok(context_outcome(
        "The model did not produce a final incomplete report after three finish requests.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };
    use wiremock::{
        Mock, MockServer, Request, Respond, ResponseTemplate,
        matchers::{method, path},
    };

    #[derive(Clone)]
    struct Sequence {
        responses: Arc<Mutex<VecDeque<ResponseTemplate>>>,
    }

    impl Sequence {
        fn new(responses: impl IntoIterator<Item = ResponseTemplate>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            }
        }
    }

    impl Respond for Sequence {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected API request")
        }
    }

    fn response(message: Value, finish_reason: &str, prompt_tokens: u64) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": message, "finish_reason": finish_reason}],
            "usage": {"prompt_tokens": prompt_tokens}
        }))
    }

    async fn server_with(responses: impl IntoIterator<Item = ResponseTemplate>) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(Sequence::new(responses))
            .mount(&server)
            .await;
        server
    }

    fn client(server: &MockServer) -> Client {
        Client::new("test-key".into(), &server.uri()).unwrap()
    }

    #[test]
    fn projection_counts_only_new_tail_when_usage_exists() {
        let mut history = vec![msg("user", "old")];
        let sent = history.len();
        history.push(Message {
            role: "assistant".into(),
            content: None,
            reasoning_content: Some("reasoning".into()),
            tool_calls: Some(vec![crate::api::ToolCall {
                id: "1".into(),
                kind: "function".into(),
                function: crate::api::FunctionCall {
                    name: "read".into(),
                    arguments: "large arguments".into(),
                },
            }]),
            tool_call_id: None,
        });
        history.push(Message {
            role: "tool".into(),
            content: Some("tool result".into()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: Some("1".into()),
        });
        assert!(projected_tokens(Some(100), &history, sent) > 100);
        assert_eq!(projected_tokens(Some(100), &history[..sent], sent), 100);
        assert!(projected_tokens(None, &history, sent) > 0);
    }

    #[tokio::test]
    async fn sends_fixed_request_and_keeps_high_usage_final_answer() {
        let server = server_with([response(
            json!({"role":"assistant","content":"final report"}),
            "stop",
            950_000,
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let outcome = run(client(&server), "task".into(), dir.path().into(), 2)
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.report.as_deref(), Some("final report"));

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], "deepseek-v4-flash");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "max");
        assert_eq!(body["max_tokens"], 131_072);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"].as_array().unwrap().len(), 4);
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("Treat tool results as authoritative"));
        assert!(system.contains("command_succeeded"));
    }

    #[tokio::test]
    async fn replays_reasoning_and_executes_same_response_tools_in_order() {
        let server = server_with([
            response(
                json!({
                    "role":"assistant",
                    "content":"",
                    "reasoning_content":"write before reading",
                    "tool_calls":[
                        {"id":"write-1","type":"function","function":{"name":"write","arguments":"{\"path\":\"ordered.txt\",\"content\":\"ready\"}"}},
                        {"id":"read-1","type":"function","function":{"name":"read","arguments":"{\"path\":\"ordered.txt\"}"}}
                    ]
                }),
                "tool_calls",
                100,
            ),
            response(
                json!({"role":"assistant","content":"done","reasoning_content":"complete"}),
                "stop",
                200,
            ),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let outcome = run(client(&server), "task".into(), dir.path().into(), 3)
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ordered.txt")).unwrap(),
            "ready"
        );

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let second: Value = serde_json::from_slice(&requests[1].body).unwrap();
        let messages = second["messages"].as_array().unwrap();
        let assistant = messages
            .iter()
            .find(|message| message["role"] == "assistant")
            .unwrap();
        assert_eq!(assistant["reasoning_content"], "write before reading");
        let outputs: Vec<_> = messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect();
        assert_eq!(outputs.len(), 2);
        assert!(outputs[1]["content"].as_str().unwrap().contains("ready"));
    }

    #[tokio::test]
    async fn max_turn_executes_work_tool_but_refuses_finish_tool() {
        let server = server_with([
            response(
                json!({
                    "role":"assistant","content":"","reasoning_content":"work",
                    "tool_calls":[{"id":"work-write","type":"function","function":{"name":"write","arguments":"{\"path\":\"work.txt\",\"content\":\"yes\"}"}}]
                }),
                "tool_calls",
                100,
            ),
            response(
                json!({
                    "role":"assistant","content":"","reasoning_content":"should not run",
                    "tool_calls":[{"id":"finish-write","type":"function","function":{"name":"write","arguments":"{\"path\":\"forbidden.txt\",\"content\":\"no\"}"}}]
                }),
                "tool_calls",
                200,
            ),
            response(
                json!({"role":"assistant","content":"## Status\nIncomplete","reasoning_content":"report"}),
                "stop",
                300,
            ),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let outcome = run(client(&server), "task".into(), dir.path().into(), 1)
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, 2);
        assert_eq!(outcome.report.as_deref(), Some("## Status\nIncomplete"));
        assert!(dir.path().join("work.txt").is_file());
        assert!(!dir.path().join("forbidden.txt").exists());

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        let third: Value = serde_json::from_slice(&requests[2].body).unwrap();
        let messages = third["messages"].as_array().unwrap();
        assert!(messages.iter().any(|message| {
            message["role"] == "assistant" && message["reasoning_content"] == "should not run"
        }));
        assert!(messages.iter().any(|message| {
            message["role"] == "tool"
                && message["tool_call_id"] == "finish-write"
                && message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("Tools are disabled"))
        }));
    }

    #[tokio::test]
    async fn context_error_returns_local_incomplete_outcome() {
        let server = server_with([ResponseTemplate::new(400)
            .set_body_string("context_length_exceeded: input is too long")])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let outcome = run(client(&server), "task".into(), dir.path().into(), 2)
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, 2);
        assert!(outcome.report.unwrap().contains("context length exceeded"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
