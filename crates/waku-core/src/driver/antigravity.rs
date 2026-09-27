//! Antigravity CLI (`agy`) streaming-JSON session.
//!
//! `agy --input-format stream-json --output-format stream-json` reads one
//! NDJSON user message per line on stdin and runs a turn for each, so one
//! process serves the whole conversation. Neither flag is in `agy --help`;
//! the wire shape below follows the community ACP bridges built on it
//! (agy-acp-map's `map-agy-to-acp.ts`), because Google ships no ACP mode yet.
//!
//! Inbound events carry an `event` tag:
//!
//! | Event | Becomes |
//! | --- | --- |
//! | `init` (`conversation_id`) | `Connected` with the resume cursor |
//! | `step_update` / `agent_response` (`text_delta`, `thought_delta`, `usage`) | `TextDelta`, `ReasoningDelta`, `UsageUpdated` |
//! | `step_update` / `tool` (`tool_info.parameters`, `output`, `error`, `state`) | `RichActivity`, keyed per turn and step |
//! | `step_update` / `error_message` | remembered as the failure summary while agy retries |
//! | `result` (`status`, `response`, `error`, `usage`) | `TurnFinished` |
//!
//! The stream has no permission requests, so Dinosaur runs agy only in Full
//! access. It has no interrupt either: Stop sends SIGINT and the next prompt
//! resumes the conversation with `--conversation <id>`, the same shape as Amp.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use anyhow::{Context as _, anyhow};
use crossbeam_channel::{Sender, unbounded};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::activity;
use crate::driver::{
    DriverControl, DriverEventSender, DriverEventSink, DriverStartOptions, SessionOptions,
};
use crate::model::{DriverEvent, ProviderResumeCursor, RuntimeMode};

const PROVIDER: &str = "Antigravity";

enum CommandMessage {
    Prompt(String),
    Shutdown,
}

pub struct AntigravityDriver {
    commands: Sender<CommandMessage>,
    active_pid: Arc<AtomicU32>,
}

/// Launch arguments. The prompt never rides here — it goes in on stdin.
pub(super) fn antigravity_args(
    model: Option<&str>,
    reasoning_effort: Option<&str>,
    conversation_id: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "--input-format".to_owned(),
        "stream-json".to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--dangerously-skip-permissions".to_owned(),
        // A turn may run for as long as the work takes.
        "--print-timeout".to_owned(),
        "0".to_owned(),
    ];
    if let Some(model) = model.filter(|model| !model.is_empty()) {
        args.extend(["--model".to_owned(), model.to_owned()]);
    }
    if let Some(effort) =
        reasoning_effort.filter(|effort| matches!(*effort, "low" | "medium" | "high"))
    {
        args.extend(["--effort".to_owned(), effort.to_owned()]);
    }
    if let Some(conversation_id) = conversation_id {
        args.extend(["--conversation".to_owned(), conversation_id.to_owned()]);
    }
    args
}

impl AntigravityDriver {
    pub fn start(options: DriverStartOptions, events: DriverEventSender) -> anyhow::Result<Self> {
        let DriverStartOptions {
            binary,
            cwd,
            mode,
            model,
            reasoning_effort,
            service_tier: _,
            context_window: _,
            agent_preset: _,
            computer_use_enabled: _,
            provider_cursor,
        } = options;
        if mode != RuntimeMode::FullAccess {
            return Err(anyhow!(tr!("errors.antigravity_full_access_only")));
        }
        let conversation_id = match provider_cursor {
            Some(ProviderResumeCursor::Antigravity { conversation_id }) => {
                (!conversation_id.is_empty()).then_some(conversation_id)
            }
            Some(cursor) => {
                return Err(anyhow!(
                    "cannot resume {PROVIDER} from a {} cursor",
                    cursor.provider().display_name()
                ));
            }
            None => None,
        };

        let mut command: Command = crate::command_env::command(&binary);
        command.current_dir(&cwd).args(antigravity_args(
            model.as_deref(),
            reasoning_effort.as_deref(),
            conversation_id.as_deref(),
        ));
        let command = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::command_env::spawn(command)
            .context("failed to start `agy` in streaming-input mode")?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("{PROVIDER} stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("{PROVIDER} stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("{PROVIDER} stderr unavailable"))?;
        let active_pid = Arc::new(AtomicU32::new(child.id()));

        if let Some(conversation_id) = conversation_id.clone() {
            let _ = events.send(DriverEvent::Connected {
                provider_cursor: Some(ProviderResumeCursor::Antigravity { conversation_id }),
            });
        }

        let (commands, command_rx) = unbounded();
        let turn_active = Arc::new(Mutex::new(false));

        let reader_events = events.clone();
        let reader_turn = turn_active.clone();
        let reader_thread = thread::Builder::new()
            .name("waku-antigravity-reader".into())
            .spawn(move || {
                let mut state = StreamState {
                    conversation_id,
                    ..StreamState::default()
                };
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    handle_event(&value, &reader_events, &reader_turn, &mut state);
                }
            })?;

        let writer_events = events.clone();
        let writer_turn = turn_active;
        thread::Builder::new()
            .name("waku-antigravity-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                while let Ok(message) = command_rx.recv() {
                    match message {
                        CommandMessage::Prompt(text) => {
                            *writer_turn.lock() = true;
                            let _ = writer_events.send(DriverEvent::TurnStarted);
                            if let Err(error) = write_line(&mut stdin, &user_message(&text)) {
                                let _ = writer_events.send(DriverEvent::Error(tr!(
                                    "errors.provider_transport_write",
                                    provider = PROVIDER,
                                    error = error
                                )));
                                if std::mem::take(&mut *writer_turn.lock()) {
                                    let _ = writer_events.send(DriverEvent::TurnFinished {
                                        success: false,
                                        summary: Some(tr!(
                                            "errors.provider_receive_prompt",
                                            provider = PROVIDER
                                        )),
                                    });
                                }
                                break;
                            }
                        }
                        CommandMessage::Shutdown => break,
                    }
                }
            })?;

        let last_visible_stderr = Arc::new(Mutex::new(None::<String>));
        let stderr_last_error = last_visible_stderr.clone();
        let stderr_events = events.clone();
        let stderr_thread = thread::Builder::new()
            .name("waku-antigravity-stderr".into())
            .spawn(move || {
                let lines = BufReader::new(stderr)
                    .lines()
                    .map_while(Result::ok)
                    .filter(|line| !line.trim().is_empty())
                    .collect::<Vec<_>>();
                if let Some(message) = super::support::provider_stderr_error(lines) {
                    let error = format!("{PROVIDER}: {message}");
                    *stderr_last_error.lock() = Some(error.clone());
                    let _ = stderr_events.send(DriverEvent::Error(error));
                }
            })?;

        let process_pid = active_pid.clone();
        thread::Builder::new()
            .name("waku-antigravity-process".into())
            .spawn(move || {
                let status = child.wait();
                process_pid.store(0, Ordering::Relaxed);
                let _ = reader_thread.join();
                let _ = stderr_thread.join();
                if let Ok(status) = status
                    && !status.success()
                    && last_visible_stderr.lock().is_none()
                {
                    let _ = events.send(DriverEvent::Error(tr!(
                        "errors.provider_exited",
                        provider = PROVIDER,
                        status = status
                    )));
                }
                let _ = events.send(DriverEvent::ProcessExited);
            })?;

        Ok(Self {
            commands,
            active_pid,
        })
    }
}

impl DriverControl for AntigravityDriver {
    fn prompt(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Prompt(prompt));
    }

    fn cancel(&self) {
        // No interrupt on the stream: end the process. The conversation lives
        // on in agy, and the next prompt resumes it with `--conversation`.
        let pid = self.active_pid.load(Ordering::Relaxed);
        if pid != 0 {
            #[cfg(unix)]
            {
                let _ = Command::new("/bin/kill")
                    .args(["-INT", &pid.to_string()])
                    .status();
            }
        }
    }

    fn respond(&self, _request_id: String, _option_id: String) {}

    fn apply_options(&self, _options: SessionOptions) -> bool {
        // Model and effort are launch arguments.
        false
    }

    fn rollback(&self, _turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        Err(anyhow!(
            "conversation rollback is not supported by this provider transport"
        ))
    }
}

impl Drop for AntigravityDriver {
    fn drop(&mut self) {
        let _ = self.commands.send(CommandMessage::Shutdown);
    }
}

fn user_message(text: &str) -> Value {
    json!({
        "event": "user",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}]
        }
    })
}

fn write_line(writer: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[derive(Default)]
struct StreamState {
    conversation_id: Option<String>,
    /// Incremented per turn: agy reuses `step_index` across turns.
    turn: u64,
    tools_seen: HashSet<u64>,
    streamed_text: bool,
    last_error: Option<String>,
}

impl StreamState {
    fn tool_id(&self, step: u64) -> String {
        format!("agy-t{}-s{step}", self.turn)
    }

    fn reset_turn(&mut self) {
        self.turn += 1;
        self.tools_seen.clear();
        self.streamed_text = false;
        self.last_error = None;
    }
}

fn string_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// A readable title for agy's tool calls: the command for `run_command`, the
/// path for file tools, otherwise the tool name.
fn tool_title(name: &str, parameters: Option<&Value>) -> String {
    if let Some(parameters) = parameters {
        if let Some(command) = string_field(parameters, &["CommandLine", "command", "cmd"]) {
            return command.split_whitespace().collect::<Vec<_>>().join(" ");
        }
        if let Some(path) = string_field(
            parameters,
            &[
                "AbsolutePath",
                "TargetFile",
                "path",
                "file_path",
                "DirectoryPath",
                "SearchPath",
            ],
        ) {
            return path.to_owned();
        }
        if let Some(query) = string_field(parameters, &["Query", "query", "SearchQuery"]) {
            return query.to_owned();
        }
    }
    name.replace(['_', '-'], " ")
}

fn error_detail(value: &Value) -> Option<String> {
    string_field(
        value,
        &["error", "message", "text", "description", "reason"],
    )
    .map(str::to_owned)
    .or_else(|| {
        value
            .pointer("/error_info/message")
            .and_then(Value::as_str)
            .map(str::to_owned)
    })
}

fn input_tokens(usage: Option<&Value>) -> Option<u64> {
    usage?.get("input_tokens")?.as_u64()
}

fn handle_event(
    value: &Value,
    events: &impl DriverEventSink,
    turn_active: &Mutex<bool>,
    state: &mut StreamState,
) {
    match value.get("event").and_then(Value::as_str) {
        Some("init") => {
            if let Some(id) = value.get("conversation_id").and_then(Value::as_str)
                && state.conversation_id.as_deref() != Some(id)
            {
                state.conversation_id = Some(id.to_owned());
                let _ = events.send(DriverEvent::Connected {
                    provider_cursor: Some(ProviderResumeCursor::Antigravity {
                        conversation_id: id.to_owned(),
                    }),
                });
            }
        }
        Some("step_update") => {
            let Some(step) = value.get("step_update") else {
                return;
            };
            let index = step.get("step_index").and_then(Value::as_u64).unwrap_or(0);
            let step_state = step.get("state").and_then(Value::as_str).unwrap_or("");
            match step.get("step_type").and_then(Value::as_str) {
                Some("agent_response") => {
                    if let Some(text) = step
                        .get("text_delta")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    {
                        state.streamed_text = true;
                        let _ = events.send(DriverEvent::TextDelta(text.to_owned()));
                    }
                    if let Some(thought) = step
                        .get("thought_delta")
                        .or_else(|| step.pointer("/agent_response/thought_delta"))
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    {
                        let _ = events.send(DriverEvent::ReasoningDelta(thought.to_owned()));
                    }
                    if let Some(tokens) = input_tokens(step.get("usage")) {
                        let _ = events.send(DriverEvent::UsageUpdated {
                            context_tokens: Some(tokens),
                            context_window: None,
                        });
                    }
                }
                Some("tool") => {
                    let info = step.get("tool_info");
                    let name = step
                        .get("tool_name")
                        .or_else(|| info.and_then(|info| info.get("name")))
                        .and_then(Value::as_str)
                        .unwrap_or("tool");
                    let parameters = info.and_then(|info| info.get("parameters"));
                    let output = info.and_then(|info| info.get("output"));
                    let error = info
                        .and_then(|info| info.get("error"))
                        .filter(|error| !error.is_null());
                    let complete = matches!(step_state, "DONE" | "ERROR");
                    let first = state.tools_seen.insert(index);
                    if !first && !complete {
                        return;
                    }
                    let _ = events.send(DriverEvent::RichActivity(activity::tool_activity(
                        Some(state.tool_id(index)),
                        super::support::classify_tool(name),
                        tool_title(name, parameters),
                        parameters,
                        error.or(output).filter(|_| complete),
                        output.filter(|_| complete),
                        step_state == "ERROR" || error.is_some(),
                        complete,
                    )));
                }
                Some("error_message") => {
                    // agy retries on its own; keep the cause for the result.
                    state.last_error = error_detail(step);
                }
                _ => {}
            }
        }
        Some("result") => {
            let result = value.get("result").unwrap_or(&Value::Null);
            if let Some(id) = result.get("conversation_id").and_then(Value::as_str)
                && state.conversation_id.as_deref() != Some(id)
            {
                state.conversation_id = Some(id.to_owned());
                let _ = events.send(DriverEvent::Connected {
                    provider_cursor: Some(ProviderResumeCursor::Antigravity {
                        conversation_id: id.to_owned(),
                    }),
                });
            }
            if let Some(tokens) = input_tokens(result.get("usage")) {
                let _ = events.send(DriverEvent::UsageUpdated {
                    context_tokens: Some(tokens),
                    context_window: None,
                });
            }
            let success = result
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status.eq_ignore_ascii_case("success"));
            let response = result
                .get("response")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty());
            if success
                && !state.streamed_text
                && let Some(response) = response
            {
                let _ = events.send(DriverEvent::TextDelta(response.to_owned()));
            }
            let summary = (!success).then(|| {
                error_detail(result)
                    .or_else(|| state.last_error.clone())
                    .or_else(|| response.map(str::to_owned))
                    .unwrap_or_else(|| tr!("errors.provider_turn_failed", provider = PROVIDER))
            });
            if let Some(summary) = &summary {
                let _ = events.send(DriverEvent::Error(format!("{PROVIDER}: {summary}")));
            }
            if std::mem::take(&mut *turn_active.lock()) {
                let _ = events.send(DriverEvent::TurnFinished { success, summary });
            }
            state.reset_turn();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::Receiver;

    fn run(lines: &[&str]) -> Vec<DriverEvent> {
        let (sender, receiver): (Sender<DriverEvent>, Receiver<DriverEvent>) = unbounded();
        let turn = Mutex::new(true);
        let mut state = StreamState::default();
        for line in lines {
            handle_event(
                &serde_json::from_str(line).unwrap(),
                &sender,
                &turn,
                &mut state,
            );
        }
        drop(sender);
        receiver.iter().collect()
    }

    #[test]
    fn args_resume_the_conversation_and_keep_prompts_off_the_command_line() {
        let args = antigravity_args(Some("gemini-3.1-pro-high"), Some("high"), Some("c1"));
        assert!(args.windows(2).any(|pair| pair == ["--conversation", "c1"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "gemini-3.1-pro-high"])
        );
        assert!(args.windows(2).any(|pair| pair == ["--effort", "high"]));
        assert!(args.contains(&"--dangerously-skip-permissions".to_owned()));
        let bare = antigravity_args(None, Some("xhigh"), None);
        assert!(!bare.contains(&"--effort".to_owned()));
        assert!(!bare.contains(&"--conversation".to_owned()));
    }

    #[test]
    fn stream_maps_text_tools_and_result() {
        let events = run(&[
            r#"{"event":"init","conversation_id":"conv-1","init":{"tools":["run_command"]}}"#,
            r#"{"event":"step_update","step_update":{"step_index":1,"step_type":"agent_response","state":"ACTIVE","thought_delta":"Thinking"}}"#,
            r#"{"event":"step_update","step_update":{"step_index":2,"step_type":"tool","state":"ACTIVE","tool_name":"run_command","tool_info":{"parameters":{"CommandLine":"ls   -la"}}}}"#,
            r#"{"event":"step_update","step_update":{"step_index":2,"step_type":"tool","state":"ACTIVE","tool_name":"run_command","tool_info":{"parameters":{"CommandLine":"ls -la"}}}}"#,
            r#"{"event":"step_update","step_update":{"step_index":2,"step_type":"tool","state":"DONE","tool_name":"run_command","tool_info":{"parameters":{"CommandLine":"ls -la"},"output":"a b"}}}"#,
            r#"{"event":"step_update","step_update":{"step_index":3,"step_type":"agent_response","state":"ACTIVE","text_delta":"Done.","usage":{"input_tokens":1200}}}"#,
            r#"{"event":"result","result":{"status":"SUCCESS","response":"Done.","conversation_id":"conv-1"}}"#,
        ]);
        assert!(
            matches!(&events[0], DriverEvent::Connected { provider_cursor: Some(ProviderResumeCursor::Antigravity { conversation_id }) } if conversation_id == "conv-1")
        );
        assert!(matches!(&events[1], DriverEvent::ReasoningDelta(text) if text == "Thinking"));
        let activities = events
            .iter()
            .filter(|event| matches!(event, DriverEvent::RichActivity(_)))
            .count();
        assert_eq!(activities, 2, "duplicate ACTIVE heartbeats are dropped");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, DriverEvent::TextDelta(text) if text == "Done."))
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, DriverEvent::TextDelta(_)))
                .count(),
            1,
            "the final response is not repeated after streaming"
        );
        assert!(matches!(
            events.last(),
            Some(DriverEvent::TurnFinished {
                success: true,
                summary: None
            })
        ));
    }

    #[test]
    fn failed_result_reports_the_retry_cause() {
        let events = run(&[
            r#"{"event":"step_update","step_update":{"step_index":1,"step_type":"error_message","message":"quota exceeded"}}"#,
            r#"{"event":"result","result":{"status":"ERROR"}}"#,
        ]);
        assert!(matches!(
            events.last(),
            Some(DriverEvent::TurnFinished { success: false, summary: Some(summary) }) if summary == "quota exceeded"
        ));
    }

    #[test]
    fn unstreamed_response_is_emitted_once() {
        let events = run(&[r#"{"event":"result","result":{"status":"SUCCESS","response":"Hi"}}"#]);
        assert!(matches!(&events[0], DriverEvent::TextDelta(text) if text == "Hi"));
    }
}
