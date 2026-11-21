#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#[path = "../agent_web.rs"]
mod agent_web;
#[path = "../agent_workspace.rs"]
mod agent_workspace;
#[path = "../desktop_backend.rs"]
pub mod desktop_backend;
#[path = "../engine_manager.rs"]
mod engine_manager;

#[path = "../media_backend.rs"]
mod media_backend;
#[path = "../schedule_backend.rs"]
mod schedule_backend;

use llama_server_launcher_rs::run_config::{
    build_server_args, lan_bind_host, probe_capabilities, RunPreferences, ServerCapabilities,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager, RunEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EngineProgress {
    engine: String,
    phase: String,
    completed: u64,
    total: u64,
    attempt: u8,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelProgress {
    download_id: String,
    model_name: String,
    completed: u64,
    total: u64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct MediaModelProgress {
    download_id: String,
    file_name: String,
    completed: u64,
    total: u64,
    overall_completed: u64,
    overall_total: u64,
}

#[derive(Clone)]
struct ModelDownloadState {
    active: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl Default for ModelDownloadState {
    fn default() -> Self {
        Self {
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[derive(Clone, Default)]
struct MediaGenerationState {
    active: Arc<Mutex<HashMap<String, Arc<media_backend::GenerationControl>>>>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeInfo {
    running: bool,
    model_name: Option<String>,
    context: String,
    port: u16,
    bind_host: String,
    last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AppUpdateInfo {
    current_version: String,
    latest_version: String,
    update_available: bool,
    release_url: String,
    release_notes: String,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Clone)]
struct RestartSpec {
    executable: PathBuf,
    args: Vec<OsString>,
    model_name: String,
    bind_host: String,
    port: u16,
    enabled: bool,
    attempts_used: u8,
    next_attempt_at: Option<Instant>,
}

const MAX_AUTO_RESTARTS: u8 = 3;
const AUTO_RESTART_BACKOFF_SECONDS: [u64; 3] = [1, 3, 8];

fn auto_restart_delay(attempts_used: u8) -> Option<Duration> {
    AUTO_RESTART_BACKOFF_SECONDS
        .get(usize::from(attempts_used))
        .copied()
        .map(Duration::from_secs)
}

fn schedule_auto_restart(spec: &mut RestartSpec) -> Option<u64> {
    let delay = auto_restart_delay(spec.attempts_used)?;
    spec.next_attempt_at = Some(Instant::now() + delay);
    Some(delay.as_secs())
}

struct RuntimeData {
    child: Option<Child>,
    model_name: Option<String>,
    context: String,
    bind_host: String,
    restart: Option<RestartSpec>,
    last_error: Option<String>,
}

struct RuntimeState {
    inner: Mutex<RuntimeData>,
    shutting_down: AtomicBool,
}

#[derive(Clone, Default)]
struct AgentScopeRegistry {
    grants: Arc<Mutex<HashMap<String, AgentScopeGrant>>>,
}

#[derive(Clone)]
struct AgentScopeGrant {
    task_id: String,
    workspace: String,
    scope: AgentPermissionScope,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AgentPermissionScope {
    ReadOnly,
    Workspace,
}

impl AgentScopeRegistry {
    fn scope_for(
        &self,
        token: &str,
        task_id: &str,
        workspace: &str,
    ) -> Result<Option<AgentPermissionScope>, String> {
        let grants = self
            .grants
            .lock()
            .map_err(|_| "Agent 权限状态锁已损坏".to_string())?;
        Ok(grants.get(token).and_then(|grant| {
            (grant.task_id == task_id && grant.workspace == workspace).then_some(grant.scope)
        }))
    }

    fn is_authorized(
        &self,
        token: &str,
        task_id: &str,
        workspace: &str,
        required_scope: AgentPermissionScope,
    ) -> Result<bool, String> {
        let scope = self.scope_for(token, task_id, workspace)?;
        Ok(matches!(scope, Some(AgentPermissionScope::Workspace))
            || (required_scope == AgentPermissionScope::ReadOnly
                && scope == Some(AgentPermissionScope::ReadOnly)))
    }

    fn grant(
        &self,
        task_id: String,
        workspace: String,
        scope: AgentPermissionScope,
    ) -> Result<String, String> {
        let mut random = [0u8; 32];
        getrandom::getrandom(&mut random)
            .map_err(|error| format!("无法生成 Agent 权限令牌：{error}"))?;
        let mut token = String::with_capacity(64);
        for byte in random {
            token.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
            token.push(char::from(b"0123456789abcdef"[(byte & 0x0f) as usize]));
        }
        let mut grants = self
            .grants
            .lock()
            .map_err(|_| "Agent 权限状态锁已损坏".to_string())?;
        if grants.len() >= 256 {
            grants.clear();
        }
        grants.insert(
            token.clone(),
            AgentScopeGrant {
                task_id,
                workspace,
                scope,
            },
        );
        Ok(token)
    }

    fn revoke(&self, task_id: &str) -> Result<(), String> {
        self.grants
            .lock()
            .map_err(|_| "Agent 权限状态锁已损坏".to_string())?
            .retain(|_, grant| grant.task_id != task_id);
        Ok(())
    }
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(RuntimeData {
                child: None,
                model_name: None,
                context: "auto".to_string(),
                bind_host: "127.0.0.1".to_string(),
                restart: None,
                last_error: None,
            }),
            shutting_down: AtomicBool::new(false),
        }
    }
}

fn stop_runtime_data(data: &mut RuntimeData) -> std::io::Result<()> {
    data.restart = None;
    if let Some(mut child) = data.child.take() {
        let should_kill = match child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) | Err(_) => true,
        };
        if should_kill {
            if let Err(error) = child.kill() {
                data.child = Some(child);
                return Err(error);
            }
        }
        if let Err(error) = child.wait() {
            data.child = Some(child);
            return Err(error);
        }
    }
    data.model_name = None;
    data.context = "auto".to_string();
    data.bind_host = "127.0.0.1".to_string();
    data.last_error = None;
    Ok(())
}

fn shutdown_runtime_state(state: &RuntimeState) -> std::io::Result<()> {
    state.shutting_down.store(true, Ordering::Release);
    let mut data = state
        .inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    stop_runtime_data(&mut data)
}

impl Drop for RuntimeState {
    fn drop(&mut self) {
        self.shutting_down.store(true, Ordering::Release);
        let data = self
            .inner
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = stop_runtime_data(data);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod runtime_shutdown_tests {
    use super::{shutdown_runtime_state, spawn_server_ready, RestartSpec, RuntimeState};
    use std::{
        fs,
        net::TcpListener,
        os::unix::fs::PermissionsExt,
        path::Path,
        process::{Child, Command},
        sync::Arc,
        thread,
        time::{Duration, Instant},
    };

    struct KillOnDrop(Child);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn process_is_present(pid: u32) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
    }

    #[test]
    fn shutdown_kills_only_the_model_child_owned_by_runtime_state() {
        let state = RuntimeState::default();
        let owned = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let owned_pid = owned.id();
        let unrelated = KillOnDrop(Command::new("/bin/sleep").arg("30").spawn().unwrap());
        let unrelated_pid = unrelated.0.id();
        {
            let mut data = state.inner.lock().unwrap();
            data.child = Some(owned);
            data.model_name = Some("test-model".to_string());
            data.restart = Some(RestartSpec {
                executable: "/bin/sleep".into(),
                args: vec!["30".into()],
                model_name: "test-model".to_string(),
                bind_host: "127.0.0.1".to_string(),
                port: 8666,
                enabled: true,
                attempts_used: 0,
                next_attempt_at: Some(Instant::now()),
            });
        }

        shutdown_runtime_state(&state).unwrap();

        assert!(
            !process_is_present(owned_pid),
            "LocM-owned model process survived shutdown"
        );
        assert!(
            process_is_present(unrelated_pid),
            "unrelated process was terminated"
        );
        let data = state.inner.lock().unwrap();
        assert!(data.child.is_none());
        assert!(data.restart.is_none());
        assert!(data.model_name.is_none());
    }

    #[test]
    fn shutdown_interrupts_startup_readiness_and_reaps_the_starting_process() {
        let state = Arc::new(RuntimeState::default());
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake-model-server");
        let pid_file = directory.path().join("server.pid");
        fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$1\"\nexec /bin/sleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();

        let startup_state = state.clone();
        let startup_executable = executable.clone();
        let startup_pid_file = pid_file.clone();
        let startup = thread::spawn(move || {
            let _data = startup_state.inner.lock().unwrap();
            spawn_server_ready(
                &startup_executable,
                &[startup_pid_file.into_os_string()],
                "127.0.0.1",
                port,
                &startup_state.shutting_down,
            )
        });

        let deadline = Instant::now() + Duration::from_secs(3);
        let pid = loop {
            if let Ok(value) = fs::read_to_string(&pid_file) {
                if let Ok(pid) = value.trim().parse::<u32>() {
                    break Some(pid);
                }
            }
            if Instant::now() >= deadline {
                break None;
            }
            thread::sleep(Duration::from_millis(10));
        };

        let shutdown_started = Instant::now();
        shutdown_runtime_state(&state).unwrap();
        assert!(shutdown_started.elapsed() < Duration::from_secs(3));
        let error = startup.join().unwrap().unwrap_err();
        assert!(error.contains("LocM 正在关闭"));
        assert!(pid.is_some(), "fake model server never started");
        assert!(
            !process_is_present(pid.unwrap()),
            "starting model process survived shutdown"
        );
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

fn agent_tool_definitions(enabled_tools: &[String]) -> serde_json::Value {
    let all = serde_json::json!([
        { "type": "function", "function": { "name": "list_files", "description": "列出所选工作区内当前目录的文件和子目录，不递归；path必须是具体相对目录，使用.表示工作区根目录，不支持*、?或glob通配符。", "parameters": { "type": "object", "properties": { "path": { "type": "string", "minLength": 1, "maxLength": 1024, "pattern": r#"^[^*?<>|\\"]+$"# } }, "required": ["path"], "additionalProperties": false } } },
        { "type": "function", "function": { "name": "search_files", "description": "在所选工作区内搜索UTF-8文本；只能使用工作区内的相对目录路径。", "parameters": { "type": "object", "properties": { "path": { "type": "string" }, "query": { "type": "string" }, "caseSensitive": { "type": "boolean" } }, "required": ["path", "query"], "additionalProperties": false } } },
        { "type": "function", "function": { "name": "read_file", "description": "读取所选工作区内的UTF-8文本文件；只能使用相对路径。", "parameters": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"], "additionalProperties": false } } },
        { "type": "function", "function": { "name": "create_folder", "description": "在所选工作区内创建空文件夹；只接受相对路径，不通过shell。", "parameters": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"], "additionalProperties": false } } },
        { "type": "function", "function": { "name": "write_file", "description": "在所选工作区内新建文件；若目标已存在，应用会先向用户请求覆盖确认。", "parameters": { "type": "object", "properties": { "path": { "type": "string" }, "content": { "type": "string" } }, "required": ["path", "content"], "additionalProperties": false } } },
        { "type": "function", "function": { "name": "edit_file", "description": "以唯一精确old_text/new_text片段修改所选工作区内的现有文件；应用会先请求用户确认。", "parameters": { "type": "object", "properties": { "path": { "type": "string" }, "old_text": { "type": "string" }, "new_text": { "type": "string" } }, "required": ["path", "old_text", "new_text"], "additionalProperties": false } } },
        { "type": "function", "function": { "name": "delete_file", "description": "删除所选工作区内的普通文件；应用会先请求用户确认。", "parameters": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"], "additionalProperties": false } } },
        { "type": "function", "function": { "name": "run_command", "description": "在所选工作区作为当前目录运行一个程序；必须提供程序名和argv参数，不使用shell；应用会先请求用户确认。", "parameters": { "type": "object", "properties": { "program": { "type": "string" }, "args": { "type": "array", "items": { "type": "string" } } }, "required": ["program", "args"], "additionalProperties": false } } },
        { "type": "function", "function": { "name": "web_search", "description": "使用 Bing RSS 搜索公开网页；搜索词会发送给外部搜索服务。", "parameters": { "type": "object", "properties": { "query": { "type": "string" } }, "required": ["query"], "additionalProperties": false } } },
        { "type": "function", "function": { "name": "web_fetch", "description": "读取公开网页的 HTML、XML 或纯文本；只允许公网 HTTP(S)，有跳转、端口、超时和响应大小限制。", "parameters": { "type": "object", "properties": { "url": { "type": "string" } }, "required": ["url"], "additionalProperties": false } } }
    ]);
    serde_json::Value::Array(
        all.as_array()
            .into_iter()
            .flatten()
            .filter(|tool| {
                tool.pointer("/function/name")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|name| enabled_tools.iter().any(|enabled| enabled == name))
            })
            .cloned()
            .collect(),
    )
}

fn agent_tool_fields(name: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    match name {
        "list_files" | "read_file" | "create_folder" | "delete_file" => {
            Some((&["path"], &["path"]))
        }
        "search_files" => Some((&["path", "query"], &["path", "query", "caseSensitive"])),
        "write_file" => Some((&["path", "content"], &["path", "content"])),
        "edit_file" => Some((
            &["path", "old_text", "new_text"],
            &["path", "old_text", "new_text"],
        )),
        "run_command" => Some((&["program", "args"], &["program", "args"])),
        "web_search" => Some((&["query"], &["query"])),
        "web_fetch" => Some((&["url"], &["url"])),
        _ => None,
    }
}

fn agent_tools_for_scope(enabled_tools: &[String], scope: &str) -> Result<Vec<String>, String> {
    match scope {
        "workspace" => Ok(enabled_tools.to_vec()),
        "read_only" => Ok(enabled_tools
            .iter()
            .filter(|name| {
                matches!(
                    name.as_str(),
                    "list_files" | "search_files" | "read_file" | "web_search" | "web_fetch"
                )
            })
            .cloned()
            .collect()),
        _ => Err("Agent 权限范围无效；只支持只读或仅限工作区".to_string()),
    }
}

fn require_agent_scope(
    permissions: &AgentScopeRegistry,
    task_id: &str,
    permission_token: &str,
    workspace: &str,
    required_scope: AgentPermissionScope,
) -> Result<(), String> {
    if task_id.is_empty() || task_id.len() > 128 || permission_token.len() != 64 {
        return Err("Agent 任务标识无效".to_string());
    }
    let canonical_workspace = agent_workspace::canonicalize_workspace(workspace)?;
    if permissions.is_authorized(
        permission_token,
        task_id,
        &canonical_workspace,
        required_scope,
    )? {
        Ok(())
    } else {
        Err(match required_scope {
            AgentPermissionScope::ReadOnly => {
                "当前任务没有经系统确认的该工作区读取权限，已拒绝列目录、搜索或读取文件".to_string()
            }
            AgentPermissionScope::Workspace => {
                "当前任务没有经系统确认的该工作区读写权限，已拒绝文件或命令副作用".to_string()
            }
        })
    }
}

fn require_agent_read_scope(
    permissions: &AgentScopeRegistry,
    task_id: &str,
    permission_token: &str,
    workspace: &str,
) -> Result<(), String> {
    require_agent_scope(
        permissions,
        task_id,
        permission_token,
        workspace,
        AgentPermissionScope::ReadOnly,
    )
}

fn require_agent_workspace_scope(
    permissions: &AgentScopeRegistry,
    task_id: &str,
    permission_token: &str,
    workspace: &str,
) -> Result<(), String> {
    require_agent_scope(
        permissions,
        task_id,
        permission_token,
        workspace,
        AgentPermissionScope::Workspace,
    )
}

fn normalize_agent_tool_call(
    call: &serde_json::Value,
    enabled_tools: &[String],
) -> Result<String, String> {
    let function = call.get("function").unwrap_or(call);
    let name = function
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "本机模型返回的工具调用缺少函数名".to_string())?;
    if !enabled_tools.iter().any(|enabled| enabled == name) {
        return Err(format!(
            "本机模型调用了已关闭的工具：{name}；此次没有执行工具"
        ));
    }
    let (required, allowed) =
        agent_tool_fields(name).ok_or_else(|| format!("本机模型请求了未开放的工具：{name}"))?;
    let raw_arguments = function
        .get("arguments")
        .or_else(|| function.get("parameters"))
        .ok_or_else(|| "本机模型返回的工具调用缺少参数".to_string())?;
    let arguments = match raw_arguments {
        serde_json::Value::String(serialized) => serde_json::from_str(serialized)
            .map_err(|error| format!("本机模型工具参数不是有效JSON：{error}"))?,
        value => value.clone(),
    };
    let arguments = arguments
        .as_object()
        .ok_or_else(|| "本机模型工具参数必须是JSON对象".to_string())?;
    if arguments.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("本机模型工具参数包含未定义字段；工具尚未执行".to_string());
    }
    if required.iter().any(|key| !arguments.contains_key(*key)) {
        return Err("本机模型工具参数缺少必填字段；工具尚未执行".to_string());
    }
    let mut action = serde_json::Map::new();
    action.insert("type".to_string(), serde_json::json!("tool_call"));
    action.insert("name".to_string(), serde_json::json!(name));
    for (key, value) in arguments {
        action.insert(key.clone(), value.clone());
    }
    serde_json::to_string(&serde_json::Value::Object(action))
        .map_err(|error| format!("无法规范化本机模型工具调用：{error}"))
}

fn normalize_agent_completion(
    response: &serde_json::Value,
    enabled_tools: &[String],
) -> Result<String, String> {
    let choice = response
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| "本机模型未返回有效的completion choice".to_string())?;
    let message = choice
        .get("message")
        .ok_or_else(|| "本机模型未返回有效的completion message".to_string())?;
    if let Some(calls) = message
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .filter(|calls| !calls.is_empty())
    {
        if calls.len() != 1 {
            return Err("本机模型一次必须只调用一个工具；此次没有执行工具".to_string());
        }
        return normalize_agent_tool_call(&calls[0], enabled_tools);
    }
    if let Some(function_call) = message.get("function_call") {
        return normalize_agent_tool_call(function_call, enabled_tools);
    }
    let content = message
        .get("content")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "本机模型既未返回文本，也未返回工具调用".to_string())?;
    if content.trim().is_empty() {
        let finish_reason = choice
            .get("finish_reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("未提供");
        return Err(format!(
            "本机模型返回了空文本（finish_reason={finish_reason}）；没有执行任何工具"
        ));
    }
    Ok(content.to_string())
}

fn agent_completion_is_empty(response: &serde_json::Value) -> bool {
    let Some(message) = response
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
    else {
        return false;
    };
    let has_tool_calls = message
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|calls| !calls.is_empty());
    let has_legacy_call = message
        .get("function_call")
        .is_some_and(|call| !call.is_null());
    let has_text = message
        .get("content")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|content| !content.trim().is_empty());
    !has_tool_calls && !has_legacy_call && !has_text
}

fn agent_completion_finish_reason(response: &serde_json::Value) -> &str {
    response
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("未提供")
}

fn retry_agent_max_tokens(max_tokens: u32, finish_reason: &str) -> u32 {
    if finish_reason != "length" {
        return max_tokens;
    }
    max_tokens.saturating_mul(2).min(4096).max(max_tokens)
}

fn build_agent_chat_payload(
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: u32,
    enabled_tools: &[String],
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "model": "local-model",
        "messages": messages,
        "temperature": temperature.clamp(0.0, 2.0),
        "max_tokens": max_tokens,
        "stream": false
    });
    if !enabled_tools.is_empty() {
        payload["tools"] = agent_tool_definitions(enabled_tools);
        payload["tool_choice"] = serde_json::json!("auto");
        payload["parallel_tool_calls"] = serde_json::json!(false);
    }
    payload
}

#[cfg(test)]
mod agent_tool_protocol_tests {
    use super::{
        agent_tool_definitions, build_agent_chat_payload, normalize_agent_completion, ChatMessage,
    };
    use serde_json::{json, Value};

    fn all_tools() -> Vec<String> {
        [
            "list_files",
            "search_files",
            "read_file",
            "create_folder",
            "write_file",
            "edit_file",
            "delete_file",
            "run_command",
        ]
        .into_iter()
        .map(str::to_string)
        .collect()
    }

    fn normalized_action(response: Value) -> Value {
        serde_json::from_str(&normalize_agent_completion(&response, &all_tools()).unwrap()).unwrap()
    }

    #[test]
    fn normalizes_openai_tool_calls_with_json_string_arguments() {
        let response = json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": { "name": "read_file", "arguments": "{\"path\":\"README.md\"}" }
                    }]
                }
            }]
        });
        assert_eq!(
            normalized_action(response),
            json!({ "type": "tool_call", "name": "read_file", "path": "README.md" })
        );
    }

    #[test]
    fn normalizes_llama_cpp_compact_tool_calls_with_object_arguments() {
        let response = json!({
            "choices": [{
                "message": {
                    "tool_calls": [{ "name": "search_files", "arguments": { "path": ".", "query": "TODO", "caseSensitive": false } }]
                }
            }]
        });
        assert_eq!(
            normalized_action(response),
            json!({ "type": "tool_call", "name": "search_files", "path": ".", "query": "TODO", "caseSensitive": false })
        );
    }

    #[test]
    fn accepts_legacy_function_call_and_empty_tool_calls_with_text() {
        let legacy = json!({ "choices": [{ "message": { "function_call": {
            "name": "create_folder", "arguments": "{\"path\":\"notes\"}"
        } } }] });
        assert_eq!(
            normalized_action(legacy),
            json!({ "type": "tool_call", "name": "create_folder", "path": "notes" })
        );
        let text = json!({ "choices": [{ "message": {
            "content": "{\"type\":\"done\",\"message\":\"ok\"}", "tool_calls": []
        } }] });
        assert_eq!(
            normalize_agent_completion(&text, &all_tools()).unwrap(),
            "{\"type\":\"done\",\"message\":\"ok\"}"
        );
    }

    #[test]
    fn rejects_empty_and_whitespace_only_model_content_with_finish_reason() {
        let empty = json!({ "choices": [{
            "finish_reason": "stop",
            "message": { "content": "" }
        }] });
        let whitespace = json!({ "choices": [{
            "finish_reason": "length",
            "message": { "content": " \n  " }
        }] });
        assert!(normalize_agent_completion(&empty, &all_tools())
            .unwrap_err()
            .contains("空文本（finish_reason=stop）"));
        assert!(normalize_agent_completion(&whitespace, &all_tools())
            .unwrap_err()
            .contains("空文本（finish_reason=length）"));
    }

    #[test]
    fn rejects_unknown_multiple_incomplete_and_extra_argument_tool_calls() {
        let call = |name: &str, arguments: Value| json!({ "choices": [{ "message": { "tool_calls": [{ "name": name, "arguments": arguments }] } }] });
        assert!(normalize_agent_completion(
            &call("run_shell", json!({ "command": "whoami" })),
            &all_tools()
        )
        .is_err());
        assert!(normalize_agent_completion(&call("read_file", json!({})), &all_tools()).is_err());
        assert!(normalize_agent_completion(
            &call("read_file", json!({ "path": "a", "outside": true })),
            &all_tools()
        )
        .is_err());
        let multiple = json!({ "choices": [{ "message": { "tool_calls": [
            { "name": "read_file", "arguments": { "path": "a" } },
            { "name": "read_file", "arguments": { "path": "b" } }
        ] } }] });
        assert!(normalize_agent_completion(&multiple, &all_tools())
            .unwrap_err()
            .contains("只调用一个工具"));
    }

    #[test]
    fn exposes_all_workspace_tools_and_keeps_plain_text_responses() {
        let enabled = all_tools();
        let tools = agent_tool_definitions(&enabled);
        let names: Vec<_> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.pointer("/function/name").and_then(Value::as_str))
            .collect();
        assert_eq!(
            names,
            [
                "list_files",
                "search_files",
                "read_file",
                "create_folder",
                "write_file",
                "edit_file",
                "delete_file",
                "run_command"
            ]
        );
        assert_eq!(
            normalize_agent_completion(&json!({ "choices": [{ "message": { "content": "{\"type\":\"done\",\"message\":\"ok\"}" } }] }), &enabled).unwrap(),
            "{\"type\":\"done\",\"message\":\"ok\"}"
        );
    }

    #[test]
    fn list_files_schema_requires_a_concrete_non_glob_path() {
        let tools = agent_tool_definitions(&["list_files".to_string()]);
        let path = &tools[0]["function"]["parameters"]["properties"]["path"];
        assert_eq!(path["minLength"], 1);
        assert_eq!(path["maxLength"], 1024);
        assert_eq!(path["pattern"], r#"^[^*?<>|\\"]+$"#);
        assert!(tools[0]["function"]["description"]
            .as_str()
            .unwrap()
            .contains("不支持*、?或glob通配符"));
        assert!(tools[0]["function"]["description"]
            .as_str()
            .unwrap()
            .contains("不递归"));
    }

    #[test]
    fn sends_tools_for_the_current_local_model_and_supports_plain_protocol_fallback() {
        let messages = [
            ChatMessage {
                role: "system".into(),
                content: "Agent system".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "Inspect the workspace".into(),
            },
        ];
        let enabled = vec!["read_file".to_string()];
        let payload = build_agent_chat_payload(&messages, 0.2, 2048, &enabled);
        assert_eq!(payload["model"], "local-model");
        assert_eq!(payload["tool_choice"], "auto");
        assert_eq!(payload["parallel_tool_calls"], false);
        assert_eq!(payload["tools"].as_array().unwrap().len(), 1);
        assert_eq!(payload["tools"][0]["function"]["name"], "read_file");
        assert_eq!(payload["messages"].as_array().unwrap().len(), 2);

        let fallback = build_agent_chat_payload(&messages, 0.2, 2048, &[]);
        assert!(fallback.get("tools").is_none());
        assert!(fallback.get("tool_choice").is_none());
        let disabled_response = json!({ "choices": [{ "message": { "tool_calls": [{ "name": "delete_file", "arguments": { "path": "a.txt" } }] } }] });
        assert!(normalize_agent_completion(&disabled_response, &enabled).is_err());
    }
}

const CHAT_STREAM_EVENT: &str = "chat-stream";
const MAX_CHAT_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_CHAT_SSE_EVENT_BYTES: usize = 128 * 1024;
const SCHEDULED_PROMPT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SCHEDULED_PROMPT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_SCHEDULED_RESPONSE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatStreamChunk {
    request_id: String,
    delta: String,
    done: bool,
}

fn parse_scheduled_response_body<R: Read>(
    body: R,
    content_length: Option<u64>,
) -> Result<ChatResponse, String> {
    if content_length.is_some_and(|length| length > MAX_SCHEDULED_RESPONSE_BYTES) {
        return Err("定时任务模型回复超过 1 MiB 安全上限".to_string());
    }
    let mut limited = body.take(MAX_SCHEDULED_RESPONSE_BYTES.saturating_add(1));
    let mut bytes = Vec::new();
    limited
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取定时任务模型回复失败：{error}"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SCHEDULED_RESPONSE_BYTES {
        return Err("定时任务模型回复超过 1 MiB 安全上限".to_string());
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("无法解析模型回复：{error}"))
}

const MAX_CHAT_HTTP_ERROR_BODY_BYTES: u64 = 4 * 1024;

fn format_chat_http_error(status: reqwest::StatusCode, body: &[u8]) -> String {
    let detail = String::from_utf8_lossy(body)
        .chars()
        .take(1000)
        .collect::<String>();
    if detail.trim().is_empty() {
        format!("本机 llama-server 返回 HTTP {status}")
    } else {
        format!("本机 llama-server 返回 HTTP {status}：{detail}")
    }
}

fn read_chat_http_error(response: reqwest::blocking::Response) -> String {
    let status = response.status();
    let mut body = Vec::new();
    let _ = response
        .take(MAX_CHAT_HTTP_ERROR_BODY_BYTES)
        .read_to_end(&mut body);
    format_chat_http_error(status, &body)
}

const AGENT_HEALTH_TIMEOUT: Duration = Duration::from_secs(3);
const AGENT_MODEL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const AGENT_MODEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const AGENT_MODEL_GUIDANCE: &str =
    "请在“运行”页面确认本地模型仍在运行且 API 可访问；仅凭此错误不能判定模型或端口的具体根因。";

fn format_agent_transport_error(stage: &str, url: &str, error: &reqwest::Error) -> String {
    let cause = if error.is_timeout() && stage == "推理请求" {
        format!(
            "连接或推理响应超时（连接上限 {} 秒、请求总上限 {} 分钟）",
            AGENT_MODEL_CONNECT_TIMEOUT.as_secs(),
            AGENT_MODEL_REQUEST_TIMEOUT.as_secs() / 60
        )
    } else if error.is_timeout() {
        format!(
            "本地健康检查超时（最多 {} 秒）",
            AGENT_HEALTH_TIMEOUT.as_secs()
        )
    } else if error.is_connect() {
        "连接失败".to_string()
    } else {
        "发送阶段失败，未获得可用 HTTP 响应".to_string()
    };
    format!(
        "Agent 本地模型{stage}发送阶段失败（{cause}），目标地址：{url}。{AGENT_MODEL_GUIDANCE} 底层错误详情：{error}"
    )
}

fn build_agent_model_client(
    connect_timeout: Duration,
    request_timeout: Duration,
) -> Result<reqwest::blocking::Client, reqwest::Error> {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .build()
}

fn format_agent_http_error(stage: &str, status: reqwest::StatusCode, body: &[u8]) -> String {
    let detail = String::from_utf8_lossy(body)
        .chars()
        .take(1000)
        .collect::<String>();
    if detail.trim().is_empty() {
        format!(
            "Agent 本地模型{stage}已收到 HTTP {status} 响应，但请求未成功。{AGENT_MODEL_GUIDANCE}"
        )
    } else {
        format!(
            "Agent 本地模型{stage}已收到 HTTP {status} 响应，但请求未成功。{AGENT_MODEL_GUIDANCE} 服务端错误详情：{detail}"
        )
    }
}

fn read_agent_http_error(response: reqwest::blocking::Response, stage: &str) -> String {
    let status = response.status();
    let mut body = Vec::new();
    let _ = response
        .take(MAX_CHAT_HTTP_ERROR_BODY_BYTES)
        .read_to_end(&mut body);
    format_agent_http_error(stage, status, &body)
}

fn check_agent_model_health(
    client: &reqwest::blocking::Client,
    host: &str,
    port: u16,
) -> Result<(), String> {
    let url = format!("http://{host}:{port}/health");
    let response = client
        .get(&url)
        .timeout(AGENT_HEALTH_TIMEOUT)
        .send()
        .map_err(|error| format_agent_transport_error("健康检查请求", &url, &error))?;
    if !response.status().is_success() {
        return Err(read_agent_http_error(response, "健康检查"));
    }
    Ok(())
}

#[cfg(test)]
mod chat_http_error_tests {
    use super::format_chat_http_error;
    use reqwest::StatusCode;

    #[test]
    fn preserves_a_bounded_server_error_detail() {
        let message = format_chat_http_error(StatusCode::BAD_REQUEST, b"context too long");
        assert!(message.contains("HTTP 400 Bad Request"));
        assert!(message.contains("context too long"));
    }

    #[test]
    fn limits_server_error_detail_and_handles_empty_bodies() {
        let body = vec![b'x'; 1200];
        let message = format_chat_http_error(StatusCode::BAD_REQUEST, &body);
        assert!(message.ends_with(&"x".repeat(1000)));
        assert!(!message.ends_with(&"x".repeat(1001)));
        assert_eq!(
            format_chat_http_error(StatusCode::BAD_REQUEST, b"  "),
            "本机 llama-server 返回 HTTP 400 Bad Request"
        );
    }
}

#[cfg(test)]
mod agent_local_model_diagnostics_tests {
    use super::{
        build_agent_model_client, check_agent_model_health, format_agent_http_error,
        format_agent_transport_error, request_agent_completion, ChatMessage,
        AGENT_MODEL_CONNECT_TIMEOUT, AGENT_MODEL_GUIDANCE, AGENT_MODEL_REQUEST_TIMEOUT,
    };
    use reqwest::{blocking::Client, StatusCode};
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::{TcpListener, TcpStream},
        thread,
        time::Duration,
    };

    fn serve_once(response: &'static str) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            stream.write_all(response.as_bytes()).unwrap();
        });
        (port, worker)
    }

    fn client() -> Client {
        build_agent_model_client(AGENT_MODEL_CONNECT_TIMEOUT, AGENT_MODEL_REQUEST_TIMEOUT)
            .expect("Agent 本地模型客户端应可构造")
    }

    fn read_http_request(stream: &TcpStream) -> (String, Vec<u8>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request_line = line.trim_end().to_string();
        let mut content_length = 0usize;
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line == "\n" || line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.trim().parse().unwrap();
                }
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
        (request_line, body)
    }

    fn write_http_response(stream: &mut TcpStream, status: &str, body: &str) {
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    }

    #[test]
    fn agent_model_client_uses_a_long_local_connect_window_and_bounded_request_timeout() {
        assert_eq!(AGENT_MODEL_CONNECT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(AGENT_MODEL_REQUEST_TIMEOUT, Duration::from_secs(30 * 60));
    }

    #[test]
    fn inference_reports_the_configured_timeout_when_a_local_server_stalls() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            thread::sleep(Duration::from_millis(150));
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        });
        let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
        let error = build_agent_model_client(Duration::from_secs(1), Duration::from_millis(20))
            .unwrap()
            .post(&url)
            .send()
            .unwrap_err();
        worker.join().unwrap();

        assert!(error.is_timeout());
        let message = format_agent_transport_error("推理请求", &url, &error);
        assert!(message.contains("连接上限 10 秒"));
        assert!(message.contains("请求总上限 30 分钟"));
        assert!(message.contains(AGENT_MODEL_GUIDANCE));
        assert!(message.contains("不能判定"));
    }

    #[test]
    fn health_probe_accepts_a_successful_local_health_response() {
        let (port, worker) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK");
        assert!(check_agent_model_health(&client(), "127.0.0.1", port).is_ok());
        worker.join().unwrap();
    }

    #[test]
    fn local_model_http_cycle_sends_tools_and_normalizes_tool_calls() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            let (mut health_stream, _) = listener.accept().unwrap();
            let (health_line, _) = read_http_request(&health_stream);
            assert_eq!(health_line, "GET /health HTTP/1.1");
            write_http_response(&mut health_stream, "200 OK", "OK");

            let (mut model_stream, _) = listener.accept().unwrap();
            let (model_line, request_body) = read_http_request(&model_stream);
            assert_eq!(model_line, "POST /v1/chat/completions HTTP/1.1");
            let payload: serde_json::Value = serde_json::from_slice(&request_body).unwrap();
            write_http_response(
                &mut model_stream,
                "200 OK",
                r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call-1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"note.txt\"}"}}]}}]}"#,
            );
            payload
        });

        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "读取 note.txt".into(),
        }];
        let enabled_tools = vec!["read_file".to_string()];
        let normalized = request_agent_completion(
            &client(),
            "127.0.0.1",
            port,
            &messages,
            0.2,
            1024,
            &enabled_tools,
        )
        .unwrap();
        let payload = worker.join().unwrap();
        assert_eq!(payload["tools"].as_array().unwrap().len(), 1);
        assert_eq!(payload["tools"][0]["function"]["name"], "read_file");
        assert_eq!(payload["tool_choice"], "auto");
        assert_eq!(payload["parallel_tool_calls"], false);
        assert_eq!(payload["messages"][0]["content"], "读取 note.txt");
        let action: serde_json::Value = serde_json::from_str(&normalized).unwrap();
        assert_eq!(action["type"], "tool_call");
        assert_eq!(action["name"], "read_file");
        assert_eq!(action["path"], "note.txt");
    }

    #[test]
    fn local_model_retries_without_tools_when_server_rejects_tool_schema() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            let (mut health_stream, _) = listener.accept().unwrap();
            let _ = read_http_request(&health_stream);
            write_http_response(&mut health_stream, "200 OK", "OK");

            let (mut tool_stream, _) = listener.accept().unwrap();
            let (_, first_body) = read_http_request(&tool_stream);
            write_http_response(&mut tool_stream, "400 Bad Request", "tools unsupported");

            let (mut fallback_stream, _) = listener.accept().unwrap();
            let (_, fallback_body) = read_http_request(&fallback_stream);
            write_http_response(
                &mut fallback_stream,
                "200 OK",
                r#"{"choices":[{"message":{"role":"assistant","content":"{\"type\":\"message\",\"message\":\"请使用 LocM 内置工具协议。\"}"}}]}"#,
            );
            let first: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
            let fallback: serde_json::Value = serde_json::from_slice(&fallback_body).unwrap();
            (first, fallback)
        });

        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "处理工作区文件".into(),
        }];
        let enabled_tools = vec!["read_file".to_string()];
        let normalized = request_agent_completion(
            &client(),
            "127.0.0.1",
            port,
            &messages,
            0.2,
            1024,
            &enabled_tools,
        )
        .unwrap();
        let (first, fallback) = worker.join().unwrap();
        assert!(first.get("tools").is_some());
        assert!(fallback.get("tools").is_none());
        assert!(fallback.get("tool_choice").is_none());
        assert!(fallback.get("parallel_tool_calls").is_none());
        assert_eq!(
            normalized,
            r#"{"type":"message","message":"请使用 LocM 内置工具协议。"}"#
        );
    }

    #[test]
    fn local_model_retries_plain_protocol_when_tool_schema_yields_empty_content() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            let (mut health_stream, _) = listener.accept().unwrap();
            let _ = read_http_request(&health_stream);
            write_http_response(&mut health_stream, "200 OK", "OK");

            let (mut tool_stream, _) = listener.accept().unwrap();
            let (_, tool_body) = read_http_request(&tool_stream);
            write_http_response(
                &mut tool_stream,
                "200 OK",
                r#"{"choices":[{"finish_reason":"stop","message":{"content":""}}]}"#,
            );

            let (mut fallback_stream, _) = listener.accept().unwrap();
            let (_, fallback_body) = read_http_request(&fallback_stream);
            write_http_response(
                &mut fallback_stream,
                "200 OK",
                r#"{"choices":[{"finish_reason":"stop","message":{"content":"{\"type\":\"tool_call\",\"name\":\"read_file\",\"path\":\"note.txt\"}"}}]}"#,
            );
            (
                serde_json::from_slice::<serde_json::Value>(&tool_body).unwrap(),
                serde_json::from_slice::<serde_json::Value>(&fallback_body).unwrap(),
            )
        });

        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "读取 note.txt".into(),
        }];
        let enabled_tools = vec!["read_file".to_string()];
        let normalized = request_agent_completion(
            &client(),
            "127.0.0.1",
            port,
            &messages,
            0.2,
            1024,
            &enabled_tools,
        )
        .unwrap();
        let (tool_payload, fallback_payload) = worker.join().unwrap();
        assert!(tool_payload.get("tools").is_some());
        assert!(fallback_payload.get("tools").is_none());
        assert_eq!(fallback_payload["max_tokens"], 1024);
        assert_eq!(
            normalized,
            r#"{"type":"tool_call","name":"read_file","path":"note.txt"}"#
        );
    }

    #[test]
    fn local_model_reports_empty_response_when_plain_protocol_retry_is_also_empty() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            let (mut health_stream, _) = listener.accept().unwrap();
            let _ = read_http_request(&health_stream);
            write_http_response(&mut health_stream, "200 OK", "OK");

            let (mut tool_stream, _) = listener.accept().unwrap();
            let _ = read_http_request(&tool_stream);
            write_http_response(
                &mut tool_stream,
                "200 OK",
                r#"{"choices":[{"finish_reason":"length","message":{"content":""}}]}"#,
            );

            let (mut fallback_stream, _) = listener.accept().unwrap();
            let (_, fallback_body) = read_http_request(&fallback_stream);
            write_http_response(
                &mut fallback_stream,
                "200 OK",
                r#"{"choices":[{"finish_reason":"length","message":{"content":"  "}}]}"#,
            );
            serde_json::from_slice::<serde_json::Value>(&fallback_body).unwrap()
        });

        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "创建网页".into(),
        }];
        let error = request_agent_completion(
            &client(),
            "127.0.0.1",
            port,
            &messages,
            0.2,
            1024,
            &["write_file".to_string()],
        )
        .unwrap_err();
        let fallback_payload = worker.join().unwrap();
        assert!(fallback_payload.get("tools").is_none());
        assert_eq!(fallback_payload["max_tokens"], 2048);
        assert!(error.contains("均返回空回复"));
        assert!(error.contains("length"));
        assert!(error.contains("max_tokens=1024"));
        assert!(error.contains("max_tokens=2048"));
        assert!(error.contains("未执行任何工具"));
    }

    #[test]
    fn health_probe_reports_http_responses_separately_from_transport_failures() {
        let (port, worker) = serve_once(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 8\r\nConnection: close\r\n\r\nbusy now",
        );
        let error = check_agent_model_health(&client(), "127.0.0.1", port).unwrap_err();
        worker.join().unwrap();
        assert!(error.contains("HTTP 503 Service Unavailable"));
        assert!(error.contains("busy now"));
        assert!(error.contains(AGENT_MODEL_GUIDANCE));
        assert!(!error.contains("发送失败"));
    }

    #[test]
    fn connection_failure_keeps_transport_detail_and_local_model_guidance() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = format!("http://127.0.0.1:{port}/health");
        let error = client().get(&url).send().unwrap_err();
        assert!(error.is_connect());
        let message = format_agent_transport_error("健康检查请求", &url, &error);
        assert!(message.contains("发送阶段失败"));
        assert!(message.contains("连接失败"));
        assert!(message.contains(&url));
        assert!(message.contains(AGENT_MODEL_GUIDANCE));
        assert!(message.contains(&error.to_string()));
    }

    #[test]
    fn inference_http_error_retains_status_and_bounded_server_detail() {
        let message =
            format_agent_http_error("推理请求", StatusCode::BAD_REQUEST, b"context too long");
        assert!(message.contains("HTTP 400 Bad Request"));
        assert!(message.contains(AGENT_MODEL_GUIDANCE));
        assert!(message.contains("context too long"));
        assert!(!message.contains("发送失败"));
    }
}

#[cfg(test)]
mod scheduled_prompt_response_tests {
    use super::{parse_scheduled_response_body, MAX_SCHEDULED_RESPONSE_BYTES};
    use std::io::Cursor;

    #[test]
    fn parses_bounded_scheduled_model_response() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"完成"}}]}"#;
        let response = parse_scheduled_response_body(Cursor::new(body.as_bytes()), None).unwrap();
        assert_eq!(response.choices[0].message.content, "完成");
    }

    #[test]
    fn rejects_advertised_oversized_scheduled_response() {
        let error = parse_scheduled_response_body(
            Cursor::new(b"{}"),
            Some(MAX_SCHEDULED_RESPONSE_BYTES + 1),
        )
        .unwrap_err();
        assert!(error.contains("1 MiB"));
    }

    #[test]
    fn rejects_oversized_scheduled_response_without_length_header() {
        let body = vec![b'x'; MAX_SCHEDULED_RESPONSE_BYTES as usize + 1];
        let error = parse_scheduled_response_body(Cursor::new(body), None).unwrap_err();
        assert!(error.contains("1 MiB"));
    }
}

fn parse_chat_sse_data(data: &str) -> Result<(String, bool), String> {
    let data = data.trim();
    if data == "[DONE]" {
        return Ok((String::new(), true));
    }
    let payload: serde_json::Value =
        serde_json::from_str(data).map_err(|error| format!("无法解析本地模型流式数据：{error}"))?;
    if let Some(error) = payload.get("error") {
        let detail = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string());
        return Err(format!("本机 llama-server 返回错误：{detail}"));
    }
    let choice = payload
        .get("choices")
        .and_then(serde_json::Value::as_array)
        .and_then(|choices| choices.first());
    let content = choice
        .and_then(|choice| choice.get("delta"))
        .and_then(|delta| delta.get("content"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            choice
                .and_then(|choice| choice.get("message"))
                .and_then(|message| message.get("content"))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            choice
                .and_then(|choice| choice.get("text"))
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or_default();
    Ok((content.to_string(), false))
}

fn dispatch_chat_sse_data<F>(
    data: &str,
    total_bytes: &mut usize,
    on_delta: &mut F,
) -> Result<bool, String>
where
    F: FnMut(&str) -> Result<(), String>,
{
    let (delta, done) = parse_chat_sse_data(data)?;
    if !delta.is_empty() {
        *total_bytes = total_bytes.saturating_add(delta.len());
        if *total_bytes > MAX_CHAT_RESPONSE_BYTES {
            return Err("本地模型回复超过 64 KiB 安全上限，已中止读取".to_string());
        }
        on_delta(&delta)?;
    }
    Ok(done)
}

fn process_chat_sse<R, F>(reader: &mut R, mut on_delta: F) -> Result<usize, String>
where
    R: BufRead,
    F: FnMut(&str) -> Result<(), String>,
{
    let mut line = Vec::new();
    let mut event_data = String::new();
    let mut total_bytes = 0usize;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| format!("读取本机模型流式回复失败：{error}"))?;
        if read == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        let line = std::str::from_utf8(&line)
            .map_err(|error| format!("本地模型流包含无效 UTF-8：{error}"))?;
        if line.is_empty() {
            if !event_data.is_empty() {
                let data = std::mem::take(&mut event_data);
                if dispatch_chat_sse_data(&data, &mut total_bytes, &mut on_delta)? {
                    return Ok(total_bytes);
                }
            }
        } else if let Some(data) = line.strip_prefix("data:") {
            if !event_data.is_empty() {
                event_data.push('\n');
            }
            event_data.push_str(data.strip_prefix(' ').unwrap_or(data));
            if event_data.len() > MAX_CHAT_SSE_EVENT_BYTES {
                return Err("本地模型 SSE 事件超过安全上限".to_string());
            }
        }
    }
    if !event_data.is_empty() {
        dispatch_chat_sse_data(&event_data, &mut total_bytes, &mut on_delta)?;
    }
    Ok(total_bytes)
}

#[cfg(test)]
mod chat_stream_tests {
    use super::{parse_chat_sse_data, process_chat_sse, MAX_CHAT_RESPONSE_BYTES};
    use std::io::{BufReader, Cursor};

    #[test]
    fn streams_utf8_deltas_and_stops_at_done() {
        let input = r#"data: {"choices":[{"delta":{"role":"assistant"},"index":0}]}

data: {"choices":[{"delta":{"content":"你"},"index":0}]}

data: {"choices":[{"delta":{"content":"好"},"index":0}]}

data: [DONE]

"#;
        let mut reader = BufReader::new(Cursor::new(input.as_bytes()));
        let mut output = String::new();
        let bytes = process_chat_sse(&mut reader, |delta| {
            output.push_str(delta);
            Ok(())
        })
        .expect("合成 SSE 应解析成功");
        assert_eq!(output, "你好");
        assert_eq!(bytes, "你好".len());
    }

    #[test]
    fn surfaces_server_errors() {
        let error = parse_chat_sse_data(r#"{"error":{"message":"context too long"}}"#)
            .expect_err("服务端错误应传递给调用方");
        assert!(error.contains("context too long"));
    }

    #[test]
    fn enforces_reply_size_limit() {
        let payload = serde_json::json!({
            "choices": [{ "delta": { "content": "x".repeat(MAX_CHAT_RESPONSE_BYTES + 1) } }]
        });
        let input = format!("data: {}\n\n", payload);
        let mut reader = BufReader::new(Cursor::new(input.as_bytes()));
        let error = process_chat_sse(&mut reader, |_| Ok(())).expect_err("超过上限的回复应被中止");
        assert!(error.contains("64 KiB"));
    }
}

#[cfg(test)]
mod update_and_recovery_tests {
    use super::{auto_restart_delay, compare_release_versions, GitHubRelease, MAX_AUTO_RESTARTS};
    use std::cmp::Ordering;

    #[test]
    fn compares_numeric_release_components() {
        assert_eq!(
            compare_release_versions("0.2.10", "0.2.9"),
            Ok(Ordering::Greater)
        );
        assert_eq!(
            compare_release_versions("1.0.0", "1.0"),
            Ok(Ordering::Equal)
        );
        assert_eq!(
            compare_release_versions("0.2.0", "0.2.1"),
            Ok(Ordering::Less)
        );
        assert!(compare_release_versions("latest", "0.2.1").is_err());
    }

    #[test]
    fn automatic_restart_uses_finite_increasing_backoff() {
        assert_eq!(auto_restart_delay(0).unwrap().as_secs(), 1);
        assert_eq!(auto_restart_delay(1).unwrap().as_secs(), 3);
        assert_eq!(auto_restart_delay(2).unwrap().as_secs(), 8);
        assert_eq!(auto_restart_delay(MAX_AUTO_RESTARTS), None);
    }

    #[test]
    fn missing_release_notes_are_accepted() {
        let release: GitHubRelease = serde_json::from_str(
            r#"{"tag_name":"v0.2.2","html_url":"https://github.com/sincalaway/locm/releases/tag/v0.2.2","body":null}"#,
        )
        .expect("GitHub allows an empty/null release description");
        assert_eq!(release.body, None);
    }
}

#[tauri::command]
fn engine_statuses() -> Result<Vec<engine_manager::EngineStatus>, String> {
    engine_manager::desktop_statuses().map_err(|error| error.to_string())
}

#[tauri::command]
fn engine_install_supported() -> bool {
    engine_manager::desktop_install_supported()
}

#[tauri::command]
async fn engine_latest(engine: String) -> Result<engine_manager::ReleaseInfo, String> {
    tauri::async_runtime::spawn_blocking(move || engine_manager::desktop_latest(&engine))
        .await
        .map_err(|error| format!("版本查询任务失败：{error}"))?
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn engine_install(
    app: AppHandle,
    engine: String,
    allow_prerelease: bool,
    expected_tag: String,
) -> Result<Vec<engine_manager::EngineStatus>, String> {
    let progress_engine = canonical_engine_id(&engine).to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let event_app = app.clone();
        engine_manager::desktop_install(&engine, allow_prerelease, &expected_tag, move |event| {
            let payload = EngineProgress {
                engine: progress_engine.clone(),
                phase: event.phase.to_string(),
                completed: event.completed,
                total: event.total,
                attempt: event.attempt,
            };
            let _ = event_app.emit("engine-progress", payload);
        })?;
        engine_manager::desktop_statuses()
    })
    .await
    .map_err(|error| format!("引擎安装任务失败：{error}"))?
    .map_err(|error| error.to_string())
}

#[tauri::command]
fn hardware_snapshot() -> desktop_backend::HardwareSnapshot {
    desktop_backend::hardware_snapshot()
}

#[tauri::command]
fn model_storage_snapshot(app: AppHandle) -> Result<desktop_backend::StorageSnapshot, String> {
    desktop_backend::model_storage_snapshot(&app)
}

#[tauri::command]
fn system_snapshot() -> desktop_backend::SystemSnapshot {
    desktop_backend::system_snapshot()
}

#[tauri::command]
fn app_settings(app: AppHandle) -> Result<desktop_backend::AppSettings, String> {
    desktop_backend::load_settings(&app)
}

#[tauri::command]
fn save_app_settings(
    app: AppHandle,
    settings: desktop_backend::AppSettings,
) -> Result<desktop_backend::AppSettings, String> {
    desktop_backend::save_settings(&app, settings)
}

#[tauri::command]
async fn check_app_update() -> Result<AppUpdateInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let endpoint = "https://api.github.com/repos/sincalaway/locm/releases/latest";
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(12))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("local-ai-workbench/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| format!("无法初始化应用版本查询：{error}"))?;
        let response = client
            .get(endpoint)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|error| format!("查询 GitHub 应用 Release 失败：{error}"))?;
        if response
            .content_length()
            .is_some_and(|length| length > 1024 * 1024)
        {
            return Err("应用版本信息响应超过 1 MiB 安全上限".to_string());
        }
        let mut bytes = Vec::new();
        response
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("读取应用版本信息失败：{error}"))?;
        if bytes.len() > 1024 * 1024 {
            return Err("应用版本信息响应超过 1 MiB 安全上限".to_string());
        }
        let release: GitHubRelease = serde_json::from_slice(&bytes)
            .map_err(|error| format!("GitHub 应用 Release 格式无效：{error}"))?;
        let latest_version = release.tag_name.trim().trim_start_matches('v').to_string();
        let current_version = env!("CARGO_PKG_VERSION").to_string();
        let update_available = compare_release_versions(&latest_version, &current_version)?
            == std::cmp::Ordering::Greater;
        let release_url = reqwest::Url::parse(&release.html_url)
            .ok()
            .filter(|url| {
                url.scheme() == "https"
                    && url.host_str() == Some("github.com")
                    && url.path().starts_with("/sincalaway/locm/releases/")
            })
            .map(|url| url.to_string())
            .unwrap_or_else(|| "https://github.com/sincalaway/locm/releases/latest".to_string());
        let release_notes: String = release
            .body
            .unwrap_or_default()
            .chars()
            .take(4000)
            .collect();
        Ok(AppUpdateInfo {
            current_version,
            latest_version,
            update_available,
            release_url,
            release_notes,
        })
    })
    .await
    .map_err(|error| format!("应用版本查询任务失败：{error}"))?
}

fn compare_release_versions(left: &str, right: &str) -> Result<std::cmp::Ordering, String> {
    fn components(version: &str) -> Option<Vec<u64>> {
        let core = version.split_once('-').map_or(version, |(core, _)| core);
        let values = core
            .split('.')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        (!values.is_empty() && values.len() <= 4).then_some(values)
    }
    let left = components(left).ok_or_else(|| format!("无法识别最新版本号：{left}"))?;
    let right = components(right).ok_or_else(|| format!("无法识别当前版本号：{right}"))?;
    let count = left.len().max(right.len());
    Ok((0..count)
        .map(|index| left.get(index).copied().unwrap_or(0))
        .cmp((0..count).map(|index| right.get(index).copied().unwrap_or(0))))
}

#[tauri::command]
fn list_models(app: AppHandle) -> Result<Vec<desktop_backend::ModelInfo>, String> {
    desktop_backend::list_models(&app)
}

#[tauri::command]
fn open_model_location(app: AppHandle, model_name: String) -> Result<(), String> {
    desktop_backend::open_model_location(&app, &model_name)
}

#[tauri::command]
fn remove_model(
    app: AppHandle,
    model_name: String,
) -> Result<Vec<desktop_backend::ModelInfo>, String> {
    let runtime = app.state::<RuntimeState>();
    let mut data = runtime
        .inner
        .lock()
        .map_err(|_| "无法确认推理服务状态，暂不删除模型".to_string())?;
    if data.model_name.as_deref() == Some(model_name.as_str()) {
        let still_running = data
            .child
            .as_mut()
            .map(|child| {
                child
                    .try_wait()
                    .map(|status| status.is_none())
                    .unwrap_or(true)
            })
            .unwrap_or(false);
        if still_running {
            return Err("该模型正在运行；请先停止模型，再移除文件".to_string());
        }
        data.child.take();
        data.model_name = None;
        data.context = "auto".to_string();
        data.restart = None;
        data.last_error = None;
    }
    desktop_backend::remove_model(&app, &model_name)
}

#[tauri::command]
fn choose_model_file(app: AppHandle) -> Result<Option<String>, String> {
    desktop_backend::pick_model_file(&app)
}

#[tauri::command]
fn choose_directory(app: AppHandle) -> Result<Option<String>, String> {
    desktop_backend::pick_directory(&app)
}

#[tauri::command]
fn agent_choose_workspace(app: AppHandle) -> Result<Option<String>, String> {
    match desktop_backend::pick_directory(&app)? {
        Some(workspace) => agent_workspace::canonicalize_workspace(&workspace).map(Some),
        None => Ok(None),
    }
}

#[tauri::command]
fn choose_llama_server_file(app: AppHandle) -> Result<Option<String>, String> {
    desktop_backend::pick_llama_server_file(&app)
}

#[tauri::command]
async fn scan_comfyui_models(
    directory: String,
) -> Result<desktop_backend::ComfyUiModelScan, String> {
    tauri::async_runtime::spawn_blocking(move || desktop_backend::scan_comfyui_models(&directory))
        .await
        .map_err(|error| format!("扫描 ComfyUI 模型目录任务失败：{error}"))?
}

#[tauri::command]
fn open_creations_directory(app: AppHandle, kind: String) -> Result<String, String> {
    desktop_backend::open_creations_directory(&app, &kind)
}

fn confirm_agent_action(app: &AppHandle, title: &str, message: &str) -> bool {
    app.dialog()
        .message(message)
        .title(title)
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::YesNo)
        .blocking_show()
}

#[tauri::command]
async fn agent_set_task_scope(
    app: AppHandle,
    task_id: String,
    permission_token: Option<String>,
    workspace: String,
    scope: String,
) -> Result<Option<String>, String> {
    if task_id.trim().is_empty() || task_id.len() > 128 {
        return Err("Agent 任务标识无效".to_string());
    }
    let permissions = app.state::<AgentScopeRegistry>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        match scope.as_str() {
            "read_only" => {
                let canonical_workspace = agent_workspace::canonicalize_workspace(&workspace)?;
                if let Some(token) = permission_token {
                    match permissions.scope_for(&token, &task_id, &canonical_workspace)? {
                        Some(AgentPermissionScope::ReadOnly) => return Ok(Some(token)),
                        Some(AgentPermissionScope::Workspace) => {
                            permissions.revoke(&task_id)?;
                            return permissions
                                .grant(task_id, canonical_workspace, AgentPermissionScope::ReadOnly)
                                .map(Some);
                        }
                        None => {}
                    }
                }
                permissions.revoke(&task_id)?;
                let message = format!(
                    "工作文件夹：{canonical_workspace}\n\n允许此 Agent 任务列出工作区内容、搜索并读取其中的文本文件。只读授权不会创建、修改或删除文件，也不会运行命令。选择“否”将拒绝工作区访问；任务会暂停且不会读取文件。"
                );
                if !confirm_agent_action(&app, "LocM Agent · 确认只读访问", &message) {
                    return Ok(None);
                }
                permissions
                    .grant(task_id, canonical_workspace, AgentPermissionScope::ReadOnly)
                    .map(Some)
            }
            "workspace" => {
                let canonical_workspace = agent_workspace::canonicalize_workspace(&workspace)?;
                if let Some(token) = permission_token {
                    if permissions.scope_for(&token, &task_id, &canonical_workspace)?
                        == Some(AgentPermissionScope::Workspace)
                    {
                        return Ok(Some(token));
                    }
                }
                permissions.revoke(&task_id)?;
                let message = format!(
                    "工作文件夹：{canonical_workspace}\n\n允许此 Agent 任务读取工作区内容，并在其中创建、写入或修改文件和文件夹；Agent 提出的删除文件或运行命令仍需单独审批。选择“否”将拒绝工作区访问，本轮会暂停且不会读取文件。若只想允许读取，请在权限菜单选择“只读”并单独确认。"
                );
                if !confirm_agent_action(&app, "LocM Agent · 确认工作区权限", &message) {
                    return Ok(None);
                }
                permissions
                    .grant(task_id, canonical_workspace, AgentPermissionScope::Workspace)
                    .map(Some)
            }
            _ => Err("Agent 权限范围无效；只支持只读或仅限工作区".to_string()),
        }
    })
    .await
    .map_err(|error| format!("确认 Agent 权限失败：{error}"))?
}

#[tauri::command]
fn agent_revoke_task_scope(app: AppHandle, task_id: String) -> Result<(), String> {
    if task_id.trim().is_empty() || task_id.len() > 128 {
        return Err("Agent 任务会话标识无效".to_string());
    }
    app.state::<AgentScopeRegistry>().revoke(&task_id)
}

fn agent_overwrite_confirmation_message(workspace: &str, path: &str, content: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(content.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let preview = content.chars().take(1_600).collect::<String>();
    let omitted = content.chars().count().saturating_sub(1_600);
    format!(
        "工作文件夹：{workspace}\n目标文件：{path}\n新内容大小：{} 字节\n新内容 SHA-256：{digest}\n\n新内容预览（最多 1600 字符）：\n{preview}{}\n\n继续后会在工作文件夹内写入或覆盖此普通文件。",
        content.len(),
        if omitted > 0 {
            format!("\n……另有 {omitted} 个字符未显示")
        } else {
            String::new()
        }
    )
}

fn agent_edit_confirmation_message(
    workspace: &str,
    path: &str,
    old_text: &str,
    new_text: &str,
) -> String {
    use sha2::{Digest, Sha256};

    fn digest(text: &str) -> String {
        Sha256::digest(text.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
    fn preview(text: &str) -> (String, usize) {
        (
            text.chars().take(1_600).collect(),
            text.chars().count().saturating_sub(1_600),
        )
    }

    let (old_preview, old_omitted) = preview(old_text);
    let (new_preview, new_omitted) = preview(new_text);
    format!(
        "工作文件夹：{workspace}\n目标文件：{path}\n旧片段大小：{} 字节\n旧片段 SHA-256：{}\n新片段大小：{} 字节\n新片段 SHA-256：{}\n\n旧文本预览（最多 1600 字符）：\n{}{}\n\n新文本预览（最多 1600 字符）：\n{}{}\n\n继续后只会把文件中唯一一处旧文本精确替换为新文本；若文件内容已变化、匹配缺失或不唯一，操作会失败。",
        old_text.len(),
        digest(old_text),
        new_text.len(),
        digest(new_text),
        if old_preview.is_empty() { "（空）" } else { &old_preview },
        if old_omitted > 0 { format!("\n……另有 {old_omitted} 个字符未显示") } else { String::new() },
        if new_preview.is_empty() { "（空）" } else { &new_preview },
        if new_omitted > 0 { format!("\n……另有 {new_omitted} 个字符未显示") } else { String::new() },
    )
}

fn agent_command_confirmation_message(workspace: &str, program: &str, args: &[String]) -> String {
    let args_json = serde_json::to_string_pretty(args).unwrap_or_else(|_| "[]".to_string());
    format!(
        "程序：{program}\n参数数组（按此顺序传递，不经 shell 拼接）：\n{args_json}\n\n工作目录：{workspace}\n\n此程序仍将以当前用户的完整权限运行，并可能读取或修改工作文件夹外的数据、访问网络或启动子进程。工作目录不是沙箱。只有在确认程序、参数和权限风险后才选择“是”。"
    )
}

#[cfg(test)]
mod agent_confirmation_tests {
    use super::{
        agent_command_confirmation_message, agent_edit_confirmation_message,
        agent_overwrite_confirmation_message, agent_tools_for_scope, require_agent_read_scope,
        require_agent_workspace_scope, AgentPermissionScope, AgentScopeRegistry,
    };

    #[test]
    fn command_confirmation_shows_exact_program_arguments_and_privilege_warning() {
        let args = vec!["--prompt".to_string(), "hello world".to_string()];
        let message = agent_command_confirmation_message("/tmp/work", "example", &args);

        assert!(message.contains("程序：example"));
        assert!(message.contains("\"hello world\""));
        assert!(message.contains("工作目录：/tmp/work"));
        assert!(message.contains("当前用户的完整权限"));
        assert!(message.contains("工作目录不是沙箱"));
    }

    #[test]
    fn overwrite_confirmation_includes_digest_preview_and_truncation_notice() {
        let content = format!("{}后", "x".repeat(1_601));
        let message = agent_overwrite_confirmation_message("/tmp/work", "note.txt", &content);

        assert!(message.contains("工作文件夹：/tmp/work"));
        assert!(message.contains("目标文件：note.txt"));
        assert!(message.contains("新内容 SHA-256："));
        assert!(message.contains("……另有 2 个字符未显示"));
    }

    #[test]
    fn edit_confirmation_includes_target_both_snippets_hashes_and_exact_match_warning() {
        let message =
            agent_edit_confirmation_message("/tmp/work", "README.md", "old title", "new title");

        assert!(message.contains("目标文件：README.md"));
        assert!(message.contains("旧文本预览"));
        assert!(message.contains("old title"));
        assert!(message.contains("新文本预览"));
        assert!(message.contains("new title"));
        assert!(message.contains("旧片段 SHA-256："));
        assert!(message.contains("新片段 SHA-256："));
        assert!(message.contains("唯一一处旧文本精确替换"));
    }

    #[test]
    fn agent_read_only_scope_filters_every_side_effect_tool() {
        let enabled = [
            "list_files",
            "search_files",
            "read_file",
            "create_folder",
            "write_file",
            "edit_file",
            "delete_file",
            "run_command",
            "web_search",
            "web_fetch",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
        assert_eq!(
            agent_tools_for_scope(&enabled, "read_only").unwrap(),
            [
                "list_files",
                "search_files",
                "read_file",
                "web_search",
                "web_fetch"
            ]
        );
        assert_eq!(
            agent_tools_for_scope(&enabled, "workspace").unwrap(),
            enabled
        );
        assert!(agent_tools_for_scope(&enabled, "unrestricted").is_err());
    }

    #[test]
    fn backend_workspace_grant_is_bound_to_task_and_canonical_workspace() {
        let registry = AgentScopeRegistry::default();
        let token = registry
            .grant(
                "task-a".to_string(),
                "/work/a".to_string(),
                AgentPermissionScope::Workspace,
            )
            .unwrap();
        assert!(registry
            .is_authorized(&token, "task-a", "/work/a", AgentPermissionScope::ReadOnly)
            .unwrap());
        assert!(registry
            .is_authorized(&token, "task-a", "/work/a", AgentPermissionScope::Workspace)
            .unwrap());
        assert!(!registry
            .is_authorized(&token, "task-a", "/work/b", AgentPermissionScope::ReadOnly)
            .unwrap());
        assert!(!registry
            .is_authorized(&token, "task-b", "/work/a", AgentPermissionScope::ReadOnly)
            .unwrap());
        assert!(!registry
            .is_authorized("", "task-a", "/work/a", AgentPermissionScope::ReadOnly)
            .unwrap());
        registry.revoke("task-a").unwrap();
        assert!(!registry
            .is_authorized(&token, "task-a", "/work/a", AgentPermissionScope::ReadOnly)
            .unwrap());
    }

    #[test]
    fn workspace_side_effect_gate_requires_the_matching_backend_grant() {
        let root = tempfile::tempdir().unwrap();
        let other = root.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let workspace = root.path().to_string_lossy().into_owned();
        let other_workspace = other.to_string_lossy().into_owned();
        let registry = AgentScopeRegistry::default();

        assert!(require_agent_workspace_scope(&registry, "task-a", "", &workspace).is_err());
        let canonical = super::agent_workspace::canonicalize_workspace(&workspace).unwrap();
        let token = registry
            .grant(
                "task-a".to_string(),
                canonical,
                AgentPermissionScope::Workspace,
            )
            .unwrap();
        assert!(require_agent_read_scope(&registry, "task-a", &token, &workspace).is_ok());
        assert!(require_agent_workspace_scope(&registry, "task-a", &token, &workspace).is_ok());
        assert!(
            require_agent_workspace_scope(&registry, "task-a", &token, &other_workspace).is_err()
        );
        assert!(require_agent_workspace_scope(&registry, "task-b", &token, &workspace).is_err());
    }

    #[test]
    fn read_only_grant_allows_reads_but_never_workspace_side_effects() {
        let root = tempfile::tempdir().unwrap();
        let other = root.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let workspace = root.path().to_string_lossy().into_owned();
        let other_workspace = other.to_string_lossy().into_owned();
        let registry = AgentScopeRegistry::default();
        let canonical = super::agent_workspace::canonicalize_workspace(&workspace).unwrap();
        let token = registry
            .grant(
                "task-a".to_string(),
                canonical,
                AgentPermissionScope::ReadOnly,
            )
            .unwrap();

        assert!(require_agent_read_scope(&registry, "task-a", &token, &workspace).is_ok());
        assert!(require_agent_workspace_scope(&registry, "task-a", &token, &workspace).is_err());
        assert!(require_agent_read_scope(&registry, "task-b", &token, &workspace).is_err());
        assert!(require_agent_read_scope(&registry, "task-a", &token, &other_workspace).is_err());
    }
}

#[tauri::command]
async fn agent_list_files(
    app: AppHandle,
    workspace: String,
    relative_path: String,
    task_id: String,
    permission_token: String,
) -> Result<Vec<agent_workspace::WorkspaceFile>, String> {
    let permissions = app.state::<AgentScopeRegistry>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        require_agent_read_scope(&permissions, &task_id, &permission_token, &workspace)?;
        agent_workspace::list_files(&workspace, &relative_path)
    })
    .await
    .map_err(|error| format!("读取工作区目录失败：{error}"))?
}

#[tauri::command]
async fn agent_search_files(
    app: AppHandle,
    workspace: String,
    relative_path: String,
    query: String,
    case_sensitive: bool,
    task_id: String,
    permission_token: String,
) -> Result<agent_workspace::SearchOutcome, String> {
    let permissions = app.state::<AgentScopeRegistry>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        require_agent_read_scope(&permissions, &task_id, &permission_token, &workspace)?;
        agent_workspace::search_files(&workspace, &relative_path, &query, case_sensitive)
    })
    .await
    .map_err(|error| format!("搜索工作区文本任务失败：{error}"))?
}

#[tauri::command]
async fn agent_read_file(
    app: AppHandle,
    workspace: String,
    relative_path: String,
    task_id: String,
    permission_token: String,
) -> Result<String, String> {
    let permissions = app.state::<AgentScopeRegistry>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        require_agent_read_scope(&permissions, &task_id, &permission_token, &workspace)?;
        agent_workspace::read_file(&workspace, &relative_path)
    })
    .await
    .map_err(|error| format!("读取工作区文件失败：{error}"))?
}

#[tauri::command]
async fn agent_create_folder(
    app: AppHandle,
    workspace: String,
    relative_path: String,
    task_id: String,
    permission_token: String,
) -> Result<agent_workspace::CreateFolderOutcome, String> {
    let permissions = app.state::<AgentScopeRegistry>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        require_agent_workspace_scope(&permissions, &task_id, &permission_token, &workspace)?;
        agent_workspace::create_folder(&workspace, &relative_path)
    })
    .await
    .map_err(|error| format!("创建工作区文件夹失败：{error}"))?
}

#[tauri::command]
async fn agent_write_file(
    app: AppHandle,
    workspace: String,
    relative_path: String,
    content: String,
    allow_overwrite: bool,
    task_id: String,
    permission_token: String,
) -> Result<agent_workspace::WriteOutcome, String> {
    let permissions = app.state::<AgentScopeRegistry>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        require_agent_workspace_scope(&permissions, &task_id, &permission_token, &workspace)?;
        if allow_overwrite
            && !confirm_agent_action(
                &app,
                "LocM Agent · 确认写入或覆盖文件",
                &agent_overwrite_confirmation_message(&workspace, &relative_path, &content),
            )
        {
            return Ok(agent_workspace::WriteOutcome {
                written: false,
                requires_confirmation: false,
                cancelled: true,
            });
        }
        agent_workspace::write_file(&workspace, &relative_path, &content, allow_overwrite)
    })
    .await
    .map_err(|error| format!("写入工作区文件失败：{error}"))?
}

#[tauri::command]
async fn agent_edit_file(
    app: AppHandle,
    workspace: String,
    relative_path: String,
    old_text: String,
    new_text: String,
    allow_edit: bool,
    task_id: String,
    permission_token: String,
) -> Result<agent_workspace::EditFileOutcome, String> {
    let permissions = app.state::<AgentScopeRegistry>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        require_agent_workspace_scope(&permissions, &task_id, &permission_token, &workspace)?;
        if allow_edit {
            let preview = agent_workspace::edit_file(
                &workspace,
                &relative_path,
                &old_text,
                &new_text,
                false,
            )?;
            if !preview.requires_confirmation {
                return Ok(preview);
            }
            if !confirm_agent_action(
                &app,
                "LocM Agent · 确认修改文件",
                &agent_edit_confirmation_message(&workspace, &relative_path, &old_text, &new_text),
            ) {
                return Ok(agent_workspace::EditFileOutcome {
                    edited: false,
                    requires_confirmation: false,
                    cancelled: true,
                });
            }
        }
        // Re-read and re-check after the native confirmation before writing.
        agent_workspace::edit_file(&workspace, &relative_path, &old_text, &new_text, allow_edit)
    })
    .await
    .map_err(|error| format!("编辑工作区文件失败：{error}"))?
}

#[tauri::command]
async fn agent_delete_file(
    app: AppHandle,
    workspace: String,
    relative_path: String,
    task_id: String,
    permission_token: String,
) -> Result<bool, String> {
    let permissions = app.state::<AgentScopeRegistry>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        require_agent_workspace_scope(&permissions, &task_id, &permission_token, &workspace)?;
        let message = format!(
            "工作文件夹：{workspace}\n将删除普通文件：{relative_path}\n\n此操作不可撤销。只有在路径与文件均正确时才选择“是”。"
        );
        if !confirm_agent_action(&app, "LocM Agent · 确认删除文件", &message) {
            return Ok(false);
        }
        agent_workspace::delete_file(&workspace, &relative_path)
            .map(|()| true)
    })
    .await
    .map_err(|error| format!("删除工作区文件失败：{error}"))?
}

#[tauri::command]
async fn agent_run_command(
    app: AppHandle,
    workspace: String,
    program: String,
    args: Vec<String>,
    task_id: String,
    permission_token: String,
) -> Result<agent_workspace::CommandOutput, String> {
    let permissions = app.state::<AgentScopeRegistry>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        require_agent_workspace_scope(&permissions, &task_id, &permission_token, &workspace)?;
        let message = agent_command_confirmation_message(&workspace, &program, &args);
        if !confirm_agent_action(&app, "LocM Agent · 确认运行命令", &message) {
            return Ok(agent_workspace::CommandOutput {
                success: false,
                timed_out: false,
                cancelled: true,
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        agent_workspace::run_command(&workspace, &program, &args)
    })
    .await
    .map_err(|error| format!("Agent 命令执行失败：{error}"))?
}

#[tauri::command]
async fn agent_web_search(query: String) -> Result<Vec<agent_web::SearchResult>, String> {
    tauri::async_runtime::spawn_blocking(move || agent_web::web_search(&query))
        .await
        .map_err(|error| format!("Agent 网页搜索任务失败：{error}"))?
}
#[tauri::command]
async fn agent_web_fetch(url: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || agent_web::web_fetch(&url))
        .await
        .map_err(|error| format!("Agent 网页读取任务失败：{error}"))?
}

#[tauri::command]
fn import_model(app: AppHandle, source: String) -> Result<Vec<desktop_backend::ModelInfo>, String> {
    desktop_backend::import_model(&app, &source)
}

#[tauri::command]
async fn search_huggingface(
    query: String,
    category: String,
) -> Result<Vec<desktop_backend::HuggingFaceModel>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        desktop_backend::search_huggingface(&query, &category)
    })
    .await
    .map_err(|error| format!("模型搜索任务失败：{error}"))?
}

#[tauri::command]
async fn huggingface_files(
    repo_id: String,
) -> Result<Vec<desktop_backend::HuggingFaceFile>, String> {
    tauri::async_runtime::spawn_blocking(move || desktop_backend::huggingface_files(&repo_id))
        .await
        .map_err(|error| format!("文件列表查询任务失败：{error}"))?
}

#[tauri::command]
async fn download_huggingface_model(
    app: AppHandle,
    state: tauri::State<'_, ModelDownloadState>,
    download_id: String,
    repo_id: String,
    file_name: String,
) -> Result<Vec<desktop_backend::ModelInfo>, String> {
    if download_id.is_empty() || download_id.len() > 128 {
        return Err("下载任务标识无效".to_string());
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let active = state.active.clone();
    {
        let mut tasks = active
            .lock()
            .map_err(|_| "无法读取下载任务状态".to_string())?;
        if !tasks.is_empty() {
            return Err("已有模型下载正在进行；请等待完成或先取消".to_string());
        }
        tasks.insert(download_id.clone(), cancelled.clone());
    }
    let event_app = app.clone();
    let event_download_id = download_id.clone();
    let model_name = format!(
        "{}__{}",
        repo_id.replace('/', "--"),
        file_name.replace('/', "__")
    );
    let result = desktop_backend::download_huggingface_model(
        &app,
        &repo_id,
        &file_name,
        cancelled.as_ref(),
        move |completed, total| {
            let _ = event_app.emit(
                "model-progress",
                ModelProgress {
                    download_id: event_download_id.clone(),
                    model_name: model_name.clone(),
                    completed,
                    total,
                },
            );
        },
    )
    .await;
    if let Ok(mut tasks) = active.lock() {
        tasks.remove(&download_id);
    }
    match result {
        Err(message) if cancelled.load(Ordering::Relaxed) => {
            Err(format!("E_DOWNLOAD_CANCELLED:{message}"))
        }
        other => other,
    }
}

#[tauri::command]
async fn list_media_models(app: AppHandle) -> Result<Vec<media_backend::MediaModelFile>, String> {
    tauri::async_runtime::spawn_blocking(move || media_backend::list_media_models(&app))
        .await
        .map_err(|error| format!("读取媒体模型清单失败：{error}"))?
}

#[tauri::command]
async fn list_generated_images(
    app: AppHandle,
) -> Result<Vec<media_backend::GeneratedImage>, String> {
    tauri::async_runtime::spawn_blocking(move || media_backend::list_generated_images(&app))
        .await
        .map_err(|error| format!("读取图片作品清单失败：{error}"))?
}

#[tauri::command]
async fn preview_generated_image(app: AppHandle, image_path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        media_backend::preview_generated_image(&app, &image_path)
    })
    .await
    .map_err(|error| format!("读取图片预览失败：{error}"))?
}

#[tauri::command]
async fn huggingface_media_files(
    repo_id: String,
) -> Result<Vec<media_backend::RemoteMediaFile>, String> {
    tauri::async_runtime::spawn_blocking(move || media_backend::huggingface_media_files(&repo_id))
        .await
        .map_err(|error| format!("查询媒体组件清单失败：{error}"))?
}

#[tauri::command]
async fn download_media_model_bundle(
    app: AppHandle,
    state: tauri::State<'_, ModelDownloadState>,
    download_id: String,
    repo_id: String,
    file_names: Vec<String>,
) -> Result<Vec<media_backend::MediaModelFile>, String> {
    if download_id.is_empty() || download_id.len() > 128 {
        return Err("下载任务标识无效".to_string());
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let active = state.active.clone();
    {
        let mut tasks = active
            .lock()
            .map_err(|_| "无法读取下载任务状态".to_string())?;
        if !tasks.is_empty() {
            return Err("已有模型下载正在进行；请等待完成或先取消".to_string());
        }
        tasks.insert(download_id.clone(), cancelled.clone());
    }
    let event_app = app.clone();
    let event_download_id = download_id.clone();
    let result = media_backend::download_media_bundle(
        &app,
        &repo_id,
        &file_names,
        cancelled.as_ref(),
        move |file_name, completed, total, overall_completed, overall_total| {
            let _ = event_app.emit(
                "media-model-progress",
                MediaModelProgress {
                    download_id: event_download_id.clone(),
                    file_name: file_name.to_string(),
                    completed,
                    total,
                    overall_completed,
                    overall_total,
                },
            );
        },
    )
    .await;
    if let Ok(mut tasks) = active.lock() {
        tasks.remove(&download_id);
    }
    match result {
        Err(message) if cancelled.load(Ordering::Relaxed) => {
            Err(format!("E_DOWNLOAD_CANCELLED:{message}"))
        }
        other => other,
    }
}

#[tauri::command]
async fn generate_media(
    app: AppHandle,
    state: tauri::State<'_, MediaGenerationState>,
    job_id: String,
    request: media_backend::MediaGenerationRequest,
) -> Result<media_backend::MediaGenerationResult, String> {
    if job_id.is_empty() || job_id.len() > 128 {
        return Err("创作任务标识无效".to_string());
    }
    let active = state.active.clone();
    let control = Arc::new(media_backend::GenerationControl::new());
    {
        let mut tasks = active
            .lock()
            .map_err(|_| "无法读取创作任务状态".to_string())?;
        if !tasks.is_empty() {
            return Err("已有画图或视频任务正在进行；请等待完成或先取消".to_string());
        }
        tasks.insert(job_id.clone(), control.clone());
    }
    let app_for_task = app.clone();
    let control_for_task = control.clone();
    let task_result = tauri::async_runtime::spawn_blocking(move || {
        media_backend::generate_media(&app_for_task, request, control_for_task)
    })
    .await
    .map_err(|error| format!("媒体生成任务异常：{error}"));
    if let Ok(mut tasks) = active.lock() {
        tasks.remove(&job_id);
    }
    task_result?
}

#[tauri::command]
fn cancel_media_generation(
    job_id: String,
    state: tauri::State<'_, MediaGenerationState>,
) -> Result<(), String> {
    let tasks = state
        .active
        .lock()
        .map_err(|_| "无法读取创作任务状态".to_string())?;
    let task = tasks
        .get(&job_id)
        .ok_or_else(|| "创作任务不存在或已结束".to_string())?;
    media_backend::cancel_generation(task);
    Ok(())
}

#[tauri::command]
fn cancel_huggingface_download(
    download_id: String,
    state: tauri::State<'_, ModelDownloadState>,
) -> Result<(), String> {
    let tasks = state
        .active
        .lock()
        .map_err(|_| "无法读取下载任务状态".to_string())?;
    let task = tasks
        .get(&download_id)
        .ok_or_else(|| "下载任务不存在或已结束".to_string())?;
    task.store(true, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
async fn server_capabilities(app: AppHandle) -> Result<ServerCapabilities, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let settings = desktop_backend::load_settings(&app)?;
        let statuses = engine_manager::desktop_statuses().map_err(|error| error.to_string())?;
        let executable = locate_llama_server(&statuses, settings.llama_server_path.as_deref())?;
        probe_capabilities(&executable)
    })
    .await
    .map_err(|error| format!("读取 llama-server 能力任务失败：{error}"))?
}

#[tauri::command]
async fn runtime_status(app: AppHandle) -> Result<RuntimeInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let settings = desktop_backend::load_settings(&app)?;
        let state = app.state::<RuntimeState>();
        runtime_status_for(&state, settings.api_port)
    })
    .await
    .map_err(|error| format!("读取推理服务状态失败：{error}"))?
}

fn runtime_status_for(state: &RuntimeState, port: u16) -> Result<RuntimeInfo, String> {
    let mut data = state
        .inner
        .lock()
        .map_err(|_| "无法读取推理服务状态".to_string())?;
    let exited = match data.child.as_mut() {
        Some(process) => process
            .try_wait()
            .map_err(|error| format!("检查推理服务失败：{error}"))?,
        None => None,
    };
    if let Some(status) = exited {
        data.child.take();
        let exited_name = data
            .model_name
            .take()
            .unwrap_or_else(|| "本地模型".to_string());
        data.last_error = Some(format!("llama-server 已退出（{status}）。"));
        let scheduled = data.restart.as_mut().and_then(|spec| {
            if spec.enabled {
                schedule_auto_restart(spec).map(|delay| (delay, spec.attempts_used + 1))
            } else {
                None
            }
        });
        if let Some((delay, attempt)) = scheduled {
            data.last_error = Some(format!(
                "模型 {exited_name} 已退出；{delay} 秒后自动重启（第 {attempt}/{MAX_AUTO_RESTARTS} 次）。"
            ));
        } else if data.restart.as_ref().is_some_and(|spec| spec.enabled) {
            data.last_error = Some(format!(
                "模型 {exited_name} 已退出；自动重启已达到最多 {MAX_AUTO_RESTARTS} 次，停止重试。"
            ));
        }
    }
    let restart_plan = data.restart.as_mut().and_then(|spec| {
        let due = spec
            .next_attempt_at
            .is_some_and(|deadline| deadline <= Instant::now());
        if spec.enabled && due && spec.attempts_used < MAX_AUTO_RESTARTS {
            spec.next_attempt_at = None;
            spec.attempts_used += 1;
            Some(spec.clone())
        } else {
            None
        }
    });
    if let Some(spec) = restart_plan {
        match spawn_server_ready(
            &spec.executable,
            &spec.args,
            &spec.bind_host,
            spec.port,
            &state.shutting_down,
        ) {
            Ok(process) => {
                data.child = Some(process);
                data.model_name = Some(spec.model_name);
                data.last_error = None;
            }
            Err(error) => {
                let retry = data.restart.as_mut().and_then(|restart| {
                    schedule_auto_restart(restart).map(|delay| (delay, restart.attempts_used + 1))
                });
                data.last_error = Some(match retry {
                    Some((delay, attempt)) => format!(
                        "模型 {} 自动重启第 {}/{} 次失败；{delay} 秒后进行第 {attempt} 次尝试：{error}",
                        spec.model_name, spec.attempts_used, MAX_AUTO_RESTARTS
                    ),
                    None => format!(
                        "模型 {} 自动重启第 {}/{} 次失败；已停止自动重试：{error}",
                        spec.model_name, spec.attempts_used, MAX_AUTO_RESTARTS
                    ),
                });
            }
        }
    }
    if data.child.is_none() {
        data.bind_host = "127.0.0.1".to_string();
    }
    Ok(RuntimeInfo {
        running: data.child.is_some(),
        model_name: data.model_name.clone(),
        context: data.context.clone(),
        port,
        bind_host: data.bind_host.clone(),
        last_error: data.last_error.clone(),
    })
}

#[tauri::command]
async fn start_local_model(
    app: AppHandle,
    model_name: String,
    options: RunPreferences,
) -> Result<RuntimeInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let settings = desktop_backend::load_settings(&app)?;
        let models = desktop_backend::list_models(&app)?;
        let model = models
            .into_iter()
            .find(|model| model.name == model_name)
            .ok_or_else(|| "所选模型不在本地模型目录中".to_string())?;
        let state = app.state::<RuntimeState>();
        if state.shutting_down.load(Ordering::Acquire) {
            return Err("LocM 正在关闭，不能启动模型".to_string());
        }
        let mut data = state
            .inner
            .lock()
            .map_err(|_| "无法锁定推理服务状态".to_string())?;
        if let Some(process) = data.child.as_mut() {
            match process.try_wait() {
                Ok(None) if data.model_name.as_deref() == Some(model_name.as_str()) => {
                    return Ok(RuntimeInfo {
                        running: true,
                        model_name: data.model_name.clone(),
                        context: data.context.clone(),
                        port: settings.api_port,
                        bind_host: data.bind_host.clone(),
                        last_error: data.last_error.clone(),
                    });
                }
                Ok(None) => return Err("已有模型正在运行；请先停止当前模型".to_string()),
                Ok(Some(_)) => {
                    data.child.take();
                    data.model_name = None;
                    data.context = "auto".to_string();
                }
                Err(error) => return Err(format!("检查现有推理服务失败：{error}")),
            }
        }
        let bind_host = if options.lan_access {
            lan_bind_host()?
        } else {
            "127.0.0.1".to_string()
        };
        let port_probe = std::net::TcpListener::bind((bind_host.as_str(), settings.api_port))
            .map_err(|error| {
                format!("API 地址 {bind_host}:{} 不可用：{error}", settings.api_port)
            })?;
        drop(port_probe);
        let statuses = engine_manager::desktop_statuses().map_err(|error| error.to_string())?;
        let executable = locate_llama_server(&statuses, settings.llama_server_path.as_deref())?;
        let capabilities = probe_capabilities(&executable)?;
        let args = build_server_args(
            Path::new(&model.path),
            &bind_host,
            settings.api_port,
            &options,
            &capabilities,
        )?;
        let process = spawn_server_ready(
            &executable,
            &args,
            &bind_host,
            settings.api_port,
            &state.shutting_down,
        )?;
        data.child = Some(process);
        data.model_name = Some(model_name.clone());
        data.context = options.context.clone();
        data.bind_host = bind_host.clone();
        data.last_error = None;
        data.restart = Some(RestartSpec {
            executable,
            args,
            model_name: model_name.clone(),
            bind_host: bind_host.clone(),
            port: settings.api_port,
            enabled: options.auto_restart,
            attempts_used: 0,
            next_attempt_at: None,
        });
        Ok(RuntimeInfo {
            running: true,
            model_name: Some(model_name),
            context: data.context.clone(),
            port: settings.api_port,
            bind_host,
            last_error: None,
        })
    })
    .await
    .map_err(|error| format!("启动任务失败：{error}"))?
}

fn spawn_server(executable: &Path, args: &[OsString]) -> Result<Child, String> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
        .spawn()
        .map_err(|error| format!("启动 llama-server 失败：{error}"))
}

fn spawn_server_ready(
    executable: &Path,
    args: &[OsString],
    host: &str,
    port: u16,
    shutting_down: &AtomicBool,
) -> Result<Child, String> {
    if shutting_down.load(Ordering::Acquire) {
        return Err("LocM 正在关闭，已取消模型启动".to_string());
    }
    let mut process = spawn_server(executable, args)?;
    if let Err(error) = wait_server_ready(&mut process, host, port, shutting_down) {
        let _ = process.kill();
        let _ = process.wait();
        return Err(error);
    }
    Ok(process)
}

fn wait_server_ready(
    process: &mut Child,
    host: &str,
    port: u16,
    shutting_down: &AtomicBool,
) -> Result<(), String> {
    let endpoint = format!("http://{host}:{port}/health");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(700))
        .build()
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        if shutting_down.load(Ordering::Acquire) {
            return Err("LocM 正在关闭，已取消模型启动".to_string());
        }
        if let Some(status) = process
            .try_wait()
            .map_err(|error| format!("检查 llama-server 状态失败：{error}"))?
        {
            return Err(format!(
                "llama-server 已退出，退出状态：{status}。请确认模型和引擎版本兼容。"
            ));
        }
        if client
            .get(&endpoint)
            .send()
            .is_ok_and(|response| response.status().is_success())
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(250));
    }
    Err("llama-server 未能在 45 秒内就绪；模型可能过大或与当前 CPU 引擎不兼容".to_string())
}

#[tauri::command]
async fn stop_local_model(app: AppHandle) -> Result<RuntimeInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let settings = desktop_backend::load_settings(&app)?;
        let state = app.state::<RuntimeState>();
        let mut data = state
            .inner
            .lock()
            .map_err(|_| "无法锁定推理服务状态".to_string())?;
        data.restart = None;
        if let Some(mut process) = data.child.take() {
            if let Err(error) = process.kill() {
                data.child = Some(process);
                return Err(format!("停止推理服务失败：{error}"));
            }
            let _ = process.wait();
        }
        data.model_name = None;
        data.context = "auto".to_string();
        data.bind_host = "127.0.0.1".to_string();
        data.last_error = None;
        Ok(RuntimeInfo {
            running: false,
            model_name: None,
            context: data.context.clone(),
            port: settings.api_port,
            bind_host: data.bind_host.clone(),
            last_error: None,
        })
    })
    .await
    .map_err(|error| format!("停止任务失败：{error}"))?
}

fn request_agent_completion(
    client: &reqwest::blocking::Client,
    host: &str,
    port: u16,
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: u32,
    agent_enabled_tools: &[String],
) -> Result<String, String> {
    check_agent_model_health(client, host, port)?;
    let url = format!("http://{host}:{port}/v1/chat/completions");
    let payload = build_agent_chat_payload(messages, temperature, max_tokens, agent_enabled_tools);
    let response = client
        .post(&url)
        .json(&payload)
        .send()
        .map_err(|error| format_agent_transport_error("推理请求", &url, &error))?;
    let mut response = response;
    let mut retried_without_tools = false;
    if !agent_enabled_tools.is_empty()
        && matches!(
            response.status(),
            reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNPROCESSABLE_ENTITY
        )
    {
        let tool_error = read_agent_http_error(response, "工具调用推理请求");
        let mut fallback_payload = payload.clone();
        if let Some(object) = fallback_payload.as_object_mut() {
            object.remove("tools");
            object.remove("tool_choice");
            object.remove("parallel_tool_calls");
        }
        let fallback = client
            .post(&url)
            .json(&fallback_payload)
            .send()
            .map_err(|error| format_agent_transport_error("兼容重试", &url, &error))?;
        if !fallback.status().is_success() {
            return Err(format!(
                "{tool_error} 不带工具定义的兼容重试也失败：{}",
                read_agent_http_error(fallback, "兼容重试")
            ));
        }
        response = fallback;
        retried_without_tools = true;
    }
    if !response.status().is_success() {
        return Err(read_agent_http_error(response, "推理请求"));
    }
    let result: serde_json::Value = response
        .json()
        .map_err(|error| format!("无法解析模型回复：{error}"))?;
    if !retried_without_tools
        && !agent_enabled_tools.is_empty()
        && agent_completion_is_empty(&result)
    {
        let first_finish_reason = agent_completion_finish_reason(&result);
        let retry_max_tokens = retry_agent_max_tokens(max_tokens, first_finish_reason);
        let mut fallback_payload = payload;
        if let Some(object) = fallback_payload.as_object_mut() {
            object.remove("tools");
            object.remove("tool_choice");
            object.remove("parallel_tool_calls");
        }
        fallback_payload["max_tokens"] = serde_json::json!(retry_max_tokens);
        let fallback = client
            .post(&url)
            .json(&fallback_payload)
            .send()
            .map_err(|error| format_agent_transport_error("空回复兼容重试", &url, &error))?;
        if !fallback.status().is_success() {
            return Err(format!(
                "本机模型返回空回复（finish_reason={first_finish_reason}）；不带工具定义的兼容重试失败：{}",
                read_agent_http_error(fallback, "空回复兼容重试")
            ));
        }
        let fallback_result: serde_json::Value = fallback
            .json()
            .map_err(|error| format!("空回复兼容重试的响应无法解析：{error}"))?;
        if agent_completion_is_empty(&fallback_result) {
            let retry_finish_reason = agent_completion_finish_reason(&fallback_result);
            return Err(format!(
                "本机模型在工具调用请求（max_tokens={max_tokens}）和纯JSON兼容重试（max_tokens={retry_max_tokens}）中均返回空回复（finish_reason={first_finish_reason}，重试={retry_finish_reason}）；未执行任何工具。若重试仍为 length，模型可能耗尽输出预算或未能结束思考通道；请检查模型上下文、聊天模板和本地服务日志。"
            ));
        }
        return normalize_agent_completion(&fallback_result, agent_enabled_tools);
    }
    normalize_agent_completion(&result, agent_enabled_tools)
}

#[tauri::command]
async fn chat_complete(
    app: AppHandle,
    messages: Vec<ChatMessage>,
    temperature: f32,
    max_tokens: u32,
    agent_enabled_tools: Vec<String>,
    agent_scope: String,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let agent_enabled_tools = agent_tools_for_scope(&agent_enabled_tools, &agent_scope)?;
        if !(1..=4096).contains(&max_tokens) {
            return Err("模型回复上限无效；必须在 1 至 4096 之间".to_string());
        }
        if messages.is_empty() || messages.len() > 60 {
            return Err("对话消息数量无效".to_string());
        }
        if messages.iter().any(|message| {
            !matches!(message.role.as_str(), "system" | "user" | "assistant")
                || message.content.len() > 32_000
        }) {
            return Err("消息角色无效或单条内容超过 32 KB".to_string());
        }
        let settings = desktop_backend::load_settings(&app)?;
        let status = runtime_status_sync(&app, settings.api_port)?;
        if !status.running {
            let detail = status
                .last_error
                .as_deref()
                .map(|error| format!("运行状态详情：{error}"))
                .unwrap_or_default();
            return Err(format!(
                "Agent 无法请求本机模型。{AGENT_MODEL_GUIDANCE} {detail}"
            ));
        }
        let client =
            build_agent_model_client(AGENT_MODEL_CONNECT_TIMEOUT, AGENT_MODEL_REQUEST_TIMEOUT)
                .map_err(|error| {
                    format!(
                    "Agent 无法初始化本机模型客户端。{AGENT_MODEL_GUIDANCE} 底层错误详情：{error}"
                )
                })?;
        request_agent_completion(
            &client,
            &status.bind_host,
            settings.api_port,
            &messages,
            temperature,
            max_tokens,
            &agent_enabled_tools,
        )
    })
    .await
    .map_err(|error| format!("对话任务失败：{error}"))?
}

#[tauri::command]
async fn chat_stream(
    app: AppHandle,
    request_id: String,
    messages: Vec<ChatMessage>,
    temperature: f32,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let valid_request_id = !request_id.is_empty()
            && request_id.len() <= 128
            && request_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        if !valid_request_id {
            return Err("聊天请求 ID 无效".to_string());
        }
        if messages.is_empty() || messages.len() > 60 {
            return Err("对话消息数量无效".to_string());
        }
        if messages.iter().any(|message| {
            !matches!(message.role.as_str(), "system" | "user" | "assistant")
                || message.content.len() > 32_000
        }) {
            return Err("消息角色无效或单条内容超过 32 KB".to_string());
        }
        if !temperature.is_finite() {
            return Err("回答随机度必须是有限数值".to_string());
        }
        let settings = desktop_backend::load_settings(&app)?;
        let status = runtime_status_sync(&app, settings.api_port)?;
        if !status.running {
            return Err("本地模型尚未启动；请先到“运行”页面启动一个 GGUF 模型".to_string());
        }
        let url = format!(
            "http://{}:{}/v1/chat/completions",
            status.bind_host, settings.api_port
        );
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30 * 60))
            .build()
            .map_err(|error| format!("无法初始化本机聊天客户端：{error}"))?;
        let response = client
            .post(url)
            .json(&serde_json::json!({
                "model": "local-model",
                "messages": messages,
                "temperature": temperature.clamp(0.0, 2.0),
                "stream": true
            }))
            .send()
            .map_err(|error| format!("连接本机 llama-server 失败：{error}"))?;
        let status_code = response.status();
        if !status_code.is_success() {
            let body = response
                .text()
                .unwrap_or_default()
                .chars()
                .take(1000)
                .collect::<String>();
            return Err(format!("本机 llama-server 返回 HTTP {status_code}：{body}"));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !content_type.contains("text/event-stream") {
            return Err(format!(
                "本机 llama-server 未返回 SSE 流（Content-Type: {}）",
                if content_type.is_empty() {
                    "未知"
                } else {
                    &content_type
                }
            ));
        }
        let request_id_for_events = request_id.clone();
        let app_for_events = app.clone();
        let mut reader = BufReader::new(response);
        let emitted_bytes = process_chat_sse(&mut reader, |delta| {
            app_for_events
                .emit(
                    CHAT_STREAM_EVENT,
                    ChatStreamChunk {
                        request_id: request_id_for_events.clone(),
                        delta: delta.to_string(),
                        done: false,
                    },
                )
                .map_err(|error| format!("无法向聊天界面发送流式内容：{error}"))
        })?;
        if emitted_bytes == 0 {
            return Err("本机 llama-server 没有返回文本内容".to_string());
        }
        app.emit(
            CHAT_STREAM_EVENT,
            ChatStreamChunk {
                request_id,
                delta: String::new(),
                done: true,
            },
        )
        .map_err(|error| format!("无法向聊天界面发送完成状态：{error}"))?;
        Ok(())
    })
    .await
    .map_err(|error| format!("本机流式对话任务失败：{error}"))?
}

#[tauri::command]
fn load_chat_sessions(app: AppHandle) -> Result<Vec<serde_json::Value>, String> {
    desktop_backend::load_chat_sessions(&app)
}

#[tauri::command]
fn save_chat_sessions(app: AppHandle, sessions: Vec<serde_json::Value>) -> Result<(), String> {
    desktop_backend::save_chat_sessions(&app, sessions)
}

#[tauri::command]
fn list_scheduled_tasks(app: AppHandle) -> Result<Vec<schedule_backend::ScheduledTask>, String> {
    schedule_backend::list(&app)
}

#[tauri::command]
fn create_scheduled_task(
    app: AppHandle,
    title: String,
    prompt: String,
    scheduled_at_ms: i64,
    recurrence: schedule_backend::Recurrence,
    interval_minutes: Option<u32>,
    notify: bool,
) -> Result<schedule_backend::ScheduledTask, String> {
    schedule_backend::create(
        &app,
        title,
        prompt,
        scheduled_at_ms,
        recurrence,
        interval_minutes,
        notify,
    )
}

#[tauri::command]
fn list_push_channels(
    state: tauri::State<'_, schedule_backend::PushChannelState>,
) -> Result<Vec<schedule_backend::PushChannelSummary>, String> {
    state.summaries()
}

#[tauri::command]
fn save_push_channel(
    state: tauri::State<'_, schedule_backend::PushChannelState>,
    channel: schedule_backend::PushChannelConfig,
) -> Result<Vec<schedule_backend::PushChannelSummary>, String> {
    state.save_channel(channel)
}

#[tauri::command]
fn delete_push_channel(
    state: tauri::State<'_, schedule_backend::PushChannelState>,
    id: String,
) -> Result<Vec<schedule_backend::PushChannelSummary>, String> {
    state.delete_channel(&id)
}

#[tauri::command]
fn set_scheduled_task_enabled(
    app: AppHandle,
    id: String,
    enabled: bool,
) -> Result<schedule_backend::ScheduledTask, String> {
    schedule_backend::set_enabled(&app, &id, enabled)
}

#[tauri::command]
fn delete_scheduled_task(app: AppHandle, id: String) -> Result<(), String> {
    schedule_backend::delete(&app, &id)
}

fn runtime_status_sync(app: &AppHandle, port: u16) -> Result<RuntimeInfo, String> {
    let state = app.state::<RuntimeState>();
    runtime_status_for(&state, port)
}

fn execute_scheduled_prompt(app: &AppHandle, prompt: &str) -> Result<String, String> {
    let settings = desktop_backend::load_settings(app)?;
    let status = runtime_status_sync(app, settings.api_port)?;
    if !status.running {
        return Err("当前没有运行中的本地模型".to_string());
    }
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(SCHEDULED_PROMPT_CONNECT_TIMEOUT)
        .timeout(SCHEDULED_PROMPT_TIMEOUT)
        .build()
        .map_err(|error| format!("无法初始化定时任务推理客户端：{error}"))?;
    let response = client
        .post(format!(
            "http://{}:{}/v1/chat/completions",
            status.bind_host, settings.api_port
        ))
        .json(&serde_json::json!({
            "model": "local-model",
            "messages": [{"role": "user", "content": prompt}],
            "temperature": 0.7,
            "stream": false
        }))
        .send()
        .map_err(|error| format!("连接本机 llama-server 失败：{error}"))?;
    if !response.status().is_success() {
        return Err(read_chat_http_error(response));
    }
    let content_length = response.content_length();
    let result = parse_scheduled_response_body(response, content_length)?;
    result
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message.content)
        .ok_or_else(|| "模型未返回回复内容".to_string())
}

fn notify_scheduled_task(
    app: &AppHandle,
    title: &str,
    status: &str,
    summary: &str,
) -> Result<(), String> {
    let channels = app.state::<schedule_backend::PushChannelState>();
    schedule_backend::send_task_notification(&channels, title, status, summary)
}

fn locate_llama_server(
    statuses: &[engine_manager::EngineStatus],
    selected_path: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(path) = selected_path {
        if !path.trim().is_empty() {
            return desktop_backend::normalize_llama_server_path(Some(path.to_string()))?
                .map(PathBuf::from)
                .ok_or_else(|| "所选 llama-server 路径为空".to_string());
        }
    }
    let chat = statuses
        .iter()
        .find(|status| status.engine == "llama.cpp")
        .ok_or_else(|| "无法读取 llama.cpp 安装状态".to_string())?;
    if chat.installed_versions.is_empty() {
        return Err("尚未安装 llama.cpp；请先到“引擎管理”安装对话引擎".to_string());
    }
    let root = PathBuf::from(&chat.install_path);
    for version in chat.installed_versions.iter().rev() {
        let version_root = root.join(&version.tag);
        if let Some(executable) = find_named_file(
            &version_root,
            if cfg!(windows) {
                "llama-server.exe"
            } else {
                "llama-server"
            },
            5,
        ) {
            return Ok(executable);
        }
    }
    Err("已安装版本中未找到 llama-server 可执行文件".to_string())
}

fn find_named_file(root: &Path, name: &str, max_depth: usize) -> Option<PathBuf> {
    fn walk(path: &Path, name: &str, depth: usize, max_depth: usize) -> Option<PathBuf> {
        if depth > max_depth {
            return None;
        }
        let entries = fs::read_dir(path).ok()?;
        for entry in entries.flatten() {
            let entry_path = entry.path();
            let file_type = entry.file_type().ok()?;
            if file_type.is_file()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(name)
            {
                return Some(entry_path);
            }
            if file_type.is_dir() && !file_type.is_symlink() {
                if let Some(found) = walk(&entry_path, name, depth + 1, max_depth) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(root, name, 0, max_depth)
}

fn canonical_engine_id(engine: &str) -> &'static str {
    match engine {
        "media" | "image-video" | "stable-diffusion.cpp" => "stable-diffusion.cpp",
        _ => "llama.cpp",
    }
}

fn main() {
    let schedule_stop = Arc::new(AtomicBool::new(false));
    let setup_schedule_stop = schedule_stop.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(RuntimeState::default())
        .manage(AgentScopeRegistry::default())
        .manage(ModelDownloadState::default())
        .manage(MediaGenerationState::default())
        .manage(schedule_backend::PushChannelState::default())
        .manage(schedule_stop.clone())
        .invoke_handler(tauri::generate_handler![
            engine_statuses,
            engine_install_supported,
            engine_latest,
            engine_install,
            hardware_snapshot,
            model_storage_snapshot,
            system_snapshot,
            app_settings,
            save_app_settings,
            list_models,
            open_model_location,
            remove_model,
            choose_model_file,
            choose_llama_server_file,
            choose_directory,
            agent_choose_workspace,
            scan_comfyui_models,
            open_creations_directory,
            agent_list_files,
            agent_search_files,
            agent_read_file,
            agent_set_task_scope,
            agent_revoke_task_scope,
            agent_create_folder,
            agent_edit_file,
            agent_write_file,
            agent_delete_file,
            agent_run_command,
            agent_web_search,
            agent_web_fetch,
            import_model,
            search_huggingface,
            huggingface_files,
            download_huggingface_model,
            list_media_models,
            list_generated_images,
            preview_generated_image,
            huggingface_media_files,
            download_media_model_bundle,
            generate_media,
            cancel_media_generation,
            cancel_huggingface_download,
            server_capabilities,
            check_app_update,
            runtime_status,
            start_local_model,
            stop_local_model,
            chat_complete,
            chat_stream,
            load_chat_sessions,
            save_chat_sessions,
            list_scheduled_tasks,
            create_scheduled_task,
            list_push_channels,
            save_push_channel,
            delete_push_channel,
            set_scheduled_task_enabled,
            delete_scheduled_task
        ])
        .setup(move |app| {
            schedule_backend::skip_missed_runs(&app.handle())?;
            schedule_backend::start_worker(
                app.handle().clone(),
                setup_schedule_stop,
                execute_scheduled_prompt,
                notify_scheduled_task,
            );
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("无法构建本地 AI 引擎管理器")
        .run(move |app_handle, event| {
            if matches!(event, RunEvent::Exit) {
                schedule_stop.store(true, Ordering::Release);
                if let Some(state) = app_handle.try_state::<RuntimeState>() {
                    if let Err(error) = shutdown_runtime_state(&state) {
                        eprintln!("LocM 关闭时停止本机模型失败：{error}");
                    }
                }
            }
        });
}
