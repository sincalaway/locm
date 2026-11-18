#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#[path = "../agent_workspace.rs"]
mod agent_workspace;
#[path = "../desktop_backend.rs"]
mod desktop_backend;
#[path = "../engine_manager.rs"]
mod engine_manager;

#[path = "../media_backend.rs"]
mod media_backend;

use llama_server_launcher_rs::run_config::{
    build_server_args, lan_bind_host, probe_capabilities, RunPreferences, ServerCapabilities,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager};

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
    port: u16,
    bind_host: String,
    last_error: Option<String>,
}

#[derive(Clone)]
struct RestartSpec {
    executable: PathBuf,
    args: Vec<OsString>,
    model_name: String,
    bind_host: String,
    port: u16,
    enabled: bool,
    attempted: bool,
}

struct RuntimeData {
    child: Option<Child>,
    model_name: Option<String>,
    bind_host: String,
    restart: Option<RestartSpec>,
    last_error: Option<String>,
}

struct RuntimeState {
    inner: Mutex<RuntimeData>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(RuntimeData {
                child: None,
                model_name: None,
                bind_host: "127.0.0.1".to_string(),
                restart: None,
                last_error: None,
            }),
        }
    }
}

impl Drop for RuntimeState {
    fn drop(&mut self) {
        if let Ok(data) = self.inner.get_mut() {
            if let Some(mut child) = data.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
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
) -> Result<Vec<engine_manager::EngineStatus>, String> {
    let progress_engine = canonical_engine_id(&engine).to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let event_app = app.clone();
        engine_manager::desktop_install(&engine, allow_prerelease, move |event| {
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

#[tauri::command]
async fn agent_workspace_path(workspace: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        agent_workspace::canonicalize_workspace(&workspace)
    })
    .await
    .map_err(|error| format!("检查 Agent 工作文件夹失败：{error}"))?
}

#[tauri::command]
async fn agent_list_files(
    workspace: String,
    relative_path: String,
) -> Result<Vec<agent_workspace::WorkspaceFile>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        agent_workspace::list_files(&workspace, &relative_path)
    })
    .await
    .map_err(|error| format!("读取工作区目录失败：{error}"))?
}

#[tauri::command]
async fn agent_read_file(workspace: String, relative_path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        agent_workspace::read_file(&workspace, &relative_path)
    })
    .await
    .map_err(|error| format!("读取工作区文件失败：{error}"))?
}

#[tauri::command]
async fn agent_write_file(
    workspace: String,
    relative_path: String,
    content: String,
    allow_overwrite: bool,
) -> Result<agent_workspace::WriteOutcome, String> {
    tauri::async_runtime::spawn_blocking(move || {
        agent_workspace::write_file(&workspace, &relative_path, &content, allow_overwrite)
    })
    .await
    .map_err(|error| format!("写入工作区文件失败：{error}"))?
}

#[tauri::command]
async fn agent_delete_file(workspace: String, relative_path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        agent_workspace::delete_file(&workspace, &relative_path)
    })
    .await
    .map_err(|error| format!("删除工作区文件失败：{error}"))?
}

#[tauri::command]
async fn agent_run_command(
    workspace: String,
    program: String,
    args: Vec<String>,
) -> Result<agent_workspace::CommandOutput, String> {
    tauri::async_runtime::spawn_blocking(move || {
        agent_workspace::run_command(&workspace, &program, &args)
    })
    .await
    .map_err(|error| format!("Agent 命令执行失败：{error}"))?
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
async fn server_capabilities() -> Result<ServerCapabilities, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let statuses = engine_manager::desktop_statuses().map_err(|error| error.to_string())?;
        let executable = locate_llama_server(&statuses)?;
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
        let restart_plan = data.restart.as_mut().and_then(|spec| {
            if spec.enabled && !spec.attempted {
                spec.attempted = true;
                Some(spec.clone())
            } else {
                None
            }
        });
        if let Some(spec) = restart_plan {
            match spawn_server_ready(&spec.executable, &spec.args, &spec.bind_host, spec.port) {
                Ok(process) => {
                    data.child = Some(process);
                    data.model_name = Some(spec.model_name);
                    data.last_error = None;
                }
                Err(error) => {
                    data.last_error = Some(format!(
                        "模型 {exited_name} 崩溃后自动重启失败；已停止自动重试：{error}"
                    ));
                }
            }
        }
    }
    if data.child.is_none() {
        data.bind_host = "127.0.0.1".to_string();
    }
    Ok(RuntimeInfo {
        running: data.child.is_some(),
        model_name: data.model_name.clone(),
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
                        port: settings.api_port,
                        bind_host: data.bind_host.clone(),
                        last_error: data.last_error.clone(),
                    });
                }
                Ok(None) => return Err("已有模型正在运行；请先停止当前模型".to_string()),
                Ok(Some(_)) => {
                    data.child.take();
                    data.model_name = None;
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
        let executable = locate_llama_server(&statuses)?;
        let capabilities = probe_capabilities(&executable)?;
        let args = build_server_args(
            Path::new(&model.path),
            &bind_host,
            settings.api_port,
            &options,
            &capabilities,
        )?;
        let process = spawn_server_ready(&executable, &args, &bind_host, settings.api_port)?;
        data.child = Some(process);
        data.model_name = Some(model_name.clone());
        data.bind_host = bind_host.clone();
        data.last_error = None;
        data.restart = Some(RestartSpec {
            executable,
            args,
            model_name: model_name.clone(),
            bind_host: bind_host.clone(),
            port: settings.api_port,
            enabled: options.auto_restart,
            attempted: false,
        });
        Ok(RuntimeInfo {
            running: true,
            model_name: Some(model_name),
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
) -> Result<Child, String> {
    let mut process = spawn_server(executable, args)?;
    if let Err(error) = wait_server_ready(&mut process, host, port) {
        let _ = process.kill();
        let _ = process.wait();
        return Err(error);
    }
    Ok(process)
}

fn wait_server_ready(process: &mut Child, host: &str, port: u16) -> Result<(), String> {
    let endpoint = format!("http://{host}:{port}/health");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(700))
        .build()
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
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
        data.bind_host = "127.0.0.1".to_string();
        data.last_error = None;
        Ok(RuntimeInfo {
            running: false,
            model_name: None,
            port: settings.api_port,
            bind_host: data.bind_host.clone(),
            last_error: None,
        })
    })
    .await
    .map_err(|error| format!("停止任务失败：{error}"))?
}

#[tauri::command]
async fn chat_complete(
    app: AppHandle,
    messages: Vec<ChatMessage>,
    temperature: f32,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
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
            return Err("本地模型尚未启动；请先到“运行”页面启动一个 GGUF 模型".to_string());
        }
        let url = format!(
            "http://{}:{}/v1/chat/completions",
            status.bind_host, settings.api_port
        );
        let response = reqwest::blocking::Client::new()
            .post(url)
            .json(&serde_json::json!({
                "model": "local-model",
                "messages": messages,
                "temperature": temperature.clamp(0.0, 2.0),
                "stream": false
            }))
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|error| format!("本地推理请求失败：{error}"))?;
        let result: ChatResponse = response
            .json()
            .map_err(|error| format!("无法解析模型回复：{error}"))?;
        result
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message.content)
            .ok_or_else(|| "模型未返回回复内容".to_string())
    })
    .await
    .map_err(|error| format!("对话任务失败：{error}"))?
}

#[tauri::command]
fn load_chat_sessions(app: AppHandle) -> Result<Vec<serde_json::Value>, String> {
    desktop_backend::load_chat_sessions(&app)
}

#[tauri::command]
fn save_chat_sessions(app: AppHandle, sessions: Vec<serde_json::Value>) -> Result<(), String> {
    desktop_backend::save_chat_sessions(&app, sessions)
}

fn runtime_status_sync(app: &AppHandle, port: u16) -> Result<RuntimeInfo, String> {
    let state = app.state::<RuntimeState>();
    runtime_status_for(&state, port)
}

fn locate_llama_server(statuses: &[engine_manager::EngineStatus]) -> Result<PathBuf, String> {
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
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(RuntimeState::default())
        .manage(ModelDownloadState::default())
        .manage(MediaGenerationState::default())
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
            choose_directory,
            scan_comfyui_models,
            open_creations_directory,
            agent_workspace_path,
            agent_list_files,
            agent_read_file,
            agent_write_file,
            agent_delete_file,
            agent_run_command,
            import_model,
            search_huggingface,
            huggingface_files,
            download_huggingface_model,
            list_media_models,
            huggingface_media_files,
            download_media_model_bundle,
            generate_media,
            cancel_media_generation,
            cancel_huggingface_download,
            server_capabilities,
            runtime_status,
            start_local_model,
            stop_local_model,
            chat_complete,
            load_chat_sessions,
            save_chat_sessions
        ])
        .run(tauri::generate_context!())
        .expect("无法启动本地 AI 引擎管理器");
}
