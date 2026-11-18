use reqwest::{blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{Read, Write},
    path::PathBuf,
    process::Command,
    sync::{Mutex, OnceLock},
    thread,
    time::Duration,
};
use sysinfo::System;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

const MAX_MODEL_DOWNLOAD_BYTES: u64 = 200 * 1024 * 1024 * 1024;
const APP_SETTINGS_FILE: &str = "settings.json";
const CHAT_HISTORY_FILE: &str = "chat-sessions.json";
const MAX_CHAT_HISTORY_BYTES: usize = 24 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareSnapshot {
    pub os: String,
    pub architecture: String,
    pub cpu_name: String,
    pub logical_cores: usize,
    pub total_memory_bytes: u64,
    pub available_memory_bytes: u64,
    pub gpu_name: Option<String>,
    pub gpu_memory_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemSnapshot {
    pub cpu_usage_percent: f32,
    pub used_memory_bytes: u64,
    pub total_memory_bytes: u64,
    pub available_memory_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HuggingFaceModel {
    pub repo_id: String,
    pub downloads: u64,
    pub likes: u64,
    pub pipeline_tag: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HuggingFaceFile {
    pub name: String,
    pub size_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub model_directory: String,
    pub appearance: String,
    pub api_port: u16,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            model_directory: String::new(),
            appearance: "system".to_string(),
            api_port: 8686,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ApiModel {
    id: String,
    #[serde(default)]
    downloads: u64,
    #[serde(default)]
    likes: u64,
    pipeline_tag: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiModelDetails {
    #[serde(default)]
    siblings: Vec<ApiSibling>,
}

#[derive(Debug, Deserialize)]
struct ApiSibling {
    rfilename: String,
    size: Option<u64>,
    lfs: Option<ApiLfs>,
}

#[derive(Debug, Deserialize)]
struct ApiLfs {
    size: Option<u64>,
}

static SYSTEM: OnceLock<Mutex<System>> = OnceLock::new();

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| format!("无法确定应用数据目录：{error}"))
}

pub fn load_chat_sessions(app: &AppHandle) -> Result<Vec<serde_json::Value>, String> {
    let path = app_data_dir(app)?.join(CHAT_HISTORY_FILE);
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("读取本机对话历史失败：{error}")),
    };
    if text.len() > MAX_CHAT_HISTORY_BYTES {
        return Err("本机对话历史超过 24 MiB 安全上限".to_string());
    }
    let sessions: Vec<serde_json::Value> =
        serde_json::from_str(&text).map_err(|error| format!("本机对话历史格式无效：{error}"))?;
    validate_chat_sessions(&sessions)?;
    Ok(sessions)
}

pub fn save_chat_sessions(app: &AppHandle, sessions: Vec<serde_json::Value>) -> Result<(), String> {
    validate_chat_sessions(&sessions)?;
    let bytes = serde_json::to_vec(&sessions).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_CHAT_HISTORY_BYTES {
        return Err("对话历史超过 24 MiB 安全上限；请删除部分较旧对话后重试".to_string());
    }
    let directory = app_data_dir(app)?;
    fs::create_dir_all(&directory).map_err(|error| format!("无法创建本机数据目录：{error}"))?;
    let path = directory.join(CHAT_HISTORY_FILE);
    let temporary = directory.join("chat-sessions.json.tmp");
    let backup = directory.join("chat-sessions.json.bak");
    fs::write(&temporary, bytes).map_err(|error| format!("写入本机对话历史失败：{error}"))?;
    if path.exists() {
        if backup.exists() {
            fs::remove_file(&backup).map_err(|error| format!("清理旧聊天历史备份失败：{error}"))?;
        }
        fs::rename(&path, &backup).map_err(|error| format!("暂存旧聊天历史失败：{error}"))?;
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        if backup.exists() {
            let _ = fs::rename(&backup, &path);
        }
        let _ = fs::remove_file(&temporary);
        return Err(format!("保存本机对话历史失败：{error}"));
    }
    if backup.exists() {
        let _ = fs::remove_file(&backup);
    }
    Ok(())
}

fn validate_chat_sessions(sessions: &[serde_json::Value]) -> Result<(), String> {
    if sessions.len() > 100 {
        return Err("最多保存 100 个本机对话".to_string());
    }
    for session in sessions {
        let id = session.get("id").and_then(serde_json::Value::as_str);
        let title = session.get("title").and_then(serde_json::Value::as_str);
        let messages = session
            .get("messages")
            .and_then(serde_json::Value::as_array);
        if id.map_or(true, str::is_empty)
            || id.is_some_and(|value| value.len() > 128)
            || title.map_or(true, |value| value.len() > 200)
            || messages.map_or(true, |value| value.len() > 1000)
        {
            return Err("本机对话记录字段或长度无效".to_string());
        }
        for message in messages.expect("validated above") {
            let role = message.get("role").and_then(serde_json::Value::as_str);
            let content = message.get("content").and_then(serde_json::Value::as_str);
            if !matches!(role, Some("user" | "assistant"))
                || content.map_or(true, |value| value.len() > 64 * 1024)
            {
                return Err("本机对话消息格式无效或内容过长".to_string());
            }
            if let Some(attachments) = message.get("attachments") {
                let Some(attachments) = attachments.as_array() else {
                    return Err("本机对话附件记录格式无效".to_string());
                };
                if attachments.len() > 5
                    || attachments.iter().any(|attachment| {
                        attachment
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .map_or(true, |name| name.len() > 300)
                            || attachment
                                .get("text")
                                .and_then(serde_json::Value::as_str)
                                .map_or(true, |text| text.len() > 32 * 1024)
                    })
                {
                    return Err("本机对话附件记录超过安全限制".to_string());
                }
            }
        }
    }
    Ok(())
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app_data_dir(app)?.join(APP_SETTINGS_FILE))
}

pub fn load_settings(app: &AppHandle) -> Result<AppSettings, String> {
    let path = settings_path(app)?;
    let mut settings = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<AppSettings>(&text)
            .map_err(|error| format!("设置文件无法解析：{error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => AppSettings::default(),
        Err(error) => return Err(format!("读取设置失败：{error}")),
    };
    if settings.model_directory.trim().is_empty() {
        settings.model_directory = app_data_dir(app)?.join("models").display().to_string();
    }
    Ok(settings)
}

pub fn save_settings(app: &AppHandle, settings: AppSettings) -> Result<AppSettings, String> {
    if !(1024..=65535).contains(&settings.api_port) {
        return Err("本地 API 端口必须在 1024 到 65535 之间".to_string());
    }
    let model_dir = PathBuf::from(&settings.model_directory);
    if model_dir.as_os_str().is_empty() {
        return Err("模型目录不能为空".to_string());
    }
    fs::create_dir_all(&model_dir).map_err(|error| format!("无法创建模型目录：{error}"))?;
    let canonical = model_dir
        .canonicalize()
        .map_err(|error| format!("无法确认模型目录：{error}"))?;
    let mut settings = settings;
    settings.model_directory = canonical.display().to_string();
    let path = settings_path(app)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("无法创建设置目录：{error}"))?;
    }
    let text = serde_json::to_vec_pretty(&settings).map_err(|error| error.to_string())?;
    fs::write(path, text).map_err(|error| format!("保存设置失败：{error}"))?;
    Ok(settings)
}

pub fn models_directory(app: &AppHandle) -> Result<PathBuf, String> {
    let settings = load_settings(app)?;
    let dir = PathBuf::from(settings.model_directory);
    fs::create_dir_all(&dir).map_err(|error| format!("无法创建模型目录：{error}"))?;
    dir.canonicalize()
        .map_err(|error| format!("无法确认模型目录：{error}"))
}

pub fn hardware_snapshot() -> HardwareSnapshot {
    let mut system = System::new_all();
    system.refresh_all();
    let cpu_name = system
        .cpus()
        .first()
        .map(|cpu| cpu.brand().trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "未识别处理器".to_string());
    let (gpu_name, gpu_memory_bytes) = detect_gpu();
    HardwareSnapshot {
        os: System::long_os_version().unwrap_or_else(|| env::consts::OS.to_string()),
        architecture: env::consts::ARCH.to_string(),
        cpu_name,
        logical_cores: system.cpus().len(),
        total_memory_bytes: system.total_memory(),
        available_memory_bytes: system.available_memory(),
        gpu_name,
        gpu_memory_bytes,
    }
}

pub fn system_snapshot() -> SystemSnapshot {
    let system = SYSTEM.get_or_init(|| Mutex::new(System::new_all()));
    let Ok(mut system) = system.lock() else {
        return SystemSnapshot {
            cpu_usage_percent: 0.0,
            used_memory_bytes: 0,
            total_memory_bytes: 0,
            available_memory_bytes: 0,
        };
    };
    system.refresh_cpu_usage();
    system.refresh_memory();
    SystemSnapshot {
        cpu_usage_percent: system.global_cpu_usage(),
        used_memory_bytes: system.used_memory(),
        total_memory_bytes: system.total_memory(),
        available_memory_bytes: system.available_memory(),
    }
}

fn detect_gpu() -> (Option<String>, Option<u64>) {
    if let Ok(output) = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        if output.status.success() {
            if let Some(line) = String::from_utf8_lossy(&output.stdout).lines().next() {
                let mut parts = line.splitn(2, ',').map(str::trim);
                let name = parts
                    .next()
                    .filter(|name| !name.is_empty())
                    .map(str::to_string);
                let memory = parts
                    .next()
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(|mib| mib.saturating_mul(1024 * 1024));
                if name.is_some() {
                    return (name, memory);
                }
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(output) = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-CimInstance Win32_VideoController | Select-Object -ExpandProperty Name",
            ])
            .output()
        {
            if output.status.success() {
                let name = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(str::to_string);
                return (name, None);
            }
        }
    }
    (None, None)
}

pub fn list_models(app: &AppHandle) -> Result<Vec<ModelInfo>, String> {
    let root = models_directory(app)?;
    let mut models = Vec::new();
    let entries = fs::read_dir(&root).map_err(|error| format!("读取模型目录失败：{error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf"))
        {
            let metadata = entry.metadata().map_err(|error| error.to_string())?;
            models.push(ModelInfo {
                name: entry.file_name().to_string_lossy().to_string(),
                path: path.display().to_string(),
                size_bytes: metadata.len(),
            });
        }
    }
    models.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()));
    Ok(models)
}

pub fn import_model(app: &AppHandle, source: &str) -> Result<Vec<ModelInfo>, String> {
    let source = PathBuf::from(source)
        .canonicalize()
        .map_err(|error| format!("无法读取所选模型文件：{error}"))?;
    if !source.is_file()
        || !source
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf"))
    {
        return Err("请选择一个 .gguf 模型文件".to_string());
    }
    let root = models_directory(app)?;
    let name = source
        .file_name()
        .ok_or_else(|| "模型文件名无效".to_string())?;
    let target = root.join(name);
    if source != target && target.exists() {
        return Err(format!("模型目录中已存在同名文件：{}", target.display()));
    }
    if source != target {
        fs::copy(&source, &target).map_err(|error| format!("导入模型失败：{error}"))?;
    }
    list_models(app)
}

pub fn open_model_location(app: &AppHandle, model_name: &str) -> Result<(), String> {
    let root = models_directory(app)?;
    let model = list_models(app)?
        .into_iter()
        .find(|model| model.name == model_name)
        .ok_or_else(|| "模型不在本地模型目录中".to_string())?;
    let path = PathBuf::from(model.path)
        .canonicalize()
        .map_err(|error| format!("无法定位模型文件：{error}"))?;
    if path.parent() != Some(root.as_path()) {
        return Err("模型文件不在本地模型目录顶层，拒绝打开该路径".to_string());
    }

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("explorer.exe");
        command.arg(format!("/select,{}", path.display()));
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg("-R").arg(&path);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(&root);
        command
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法打开模型所在位置：{error}"))
}

pub fn remove_model(app: &AppHandle, model_name: &str) -> Result<Vec<ModelInfo>, String> {
    let root = models_directory(app)?;
    let model = list_models(app)?
        .into_iter()
        .find(|model| model.name == model_name)
        .ok_or_else(|| "模型不在本地模型目录中".to_string())?;
    let path = PathBuf::from(model.path)
        .canonicalize()
        .map_err(|error| format!("无法定位模型文件：{error}"))?;
    if path.parent() != Some(root.as_path())
        || !path.is_file()
        || !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("gguf"))
    {
        return Err("目标不是模型目录中的 GGUF 文件，拒绝删除".to_string());
    }
    fs::remove_file(&path).map_err(|error| format!("删除模型失败：{error}"))?;
    list_models(app)
}

pub fn pick_model_file(app: &AppHandle) -> Result<Option<String>, String> {
    app.dialog()
        .file()
        .add_filter("GGUF 模型", &["gguf"])
        .blocking_pick_file()
        .map(|path| path.into_path().map(|path| path.display().to_string()))
        .transpose()
        .map_err(|error| format!("无法读取所选文件路径：{error}"))
}

pub fn pick_directory(app: &AppHandle) -> Result<Option<String>, String> {
    app.dialog()
        .file()
        .blocking_pick_folder()
        .map(|path| path.into_path().map(|path| path.display().to_string()))
        .transpose()
        .map_err(|error| format!("无法读取所选目录：{error}"))
}

pub fn open_creations_directory(app: &AppHandle, kind: &str) -> Result<String, String> {
    let subdirectory = match kind {
        "image" => "images",
        "video" => "videos",
        _ => return Err("作品类型无效；仅支持 image 或 video".to_string()),
    };
    let directory = app_data_dir(app)?.join("creations").join(subdirectory);
    fs::create_dir_all(&directory).map_err(|error| format!("无法创建作品目录：{error}"))?;
    let directory = directory
        .canonicalize()
        .map_err(|error| format!("无法确认作品目录：{error}"))?;

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("explorer.exe");
        command.arg(&directory);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(&directory);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(&directory);
        command
    };
    command
        .spawn()
        .map_err(|error| format!("无法打开作品目录：{error}"))?;
    Ok(directory.display().to_string())
}

fn hf_client() -> Result<Client, String> {
    let allow = |attempt: reqwest::redirect::Attempt<'_>| {
        let url = attempt.url();
        if url.scheme() == "https" && is_huggingface_host(url.host_str().unwrap_or_default()) {
            attempt.follow()
        } else {
            attempt.error("拒绝跳转到非 Hugging Face 官方 HTTPS 域名")
        }
    };
    Client::builder()
        .user_agent(concat!("local-engine-manager/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(60 * 60))
        .redirect(Policy::custom(allow))
        .build()
        .map_err(|error| error.to_string())
}

fn is_huggingface_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("huggingface.co")
        || host.to_ascii_lowercase().ends_with(".huggingface.co")
        || host.eq_ignore_ascii_case("hf.co")
        || host.to_ascii_lowercase().ends_with(".hf.co")
}

fn hf_get(url: &str, action: &str) -> Result<reqwest::blocking::Response, String> {
    let client =
        hf_client().map_err(|error| format!("无法初始化 Hugging Face 网络客户端：{error}"))?;
    for attempt in 1u64..=3 {
        match client.get(url).send() {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => {
                let status = response.status();
                if attempt < 3 && (status.as_u16() == 429 || status.is_server_error()) {
                    thread::sleep(Duration::from_millis(350 * u64::from(attempt)));
                    continue;
                }
                let detail = match status.as_u16() {
                    401 | 403 | 404 => "仓库可能不存在、设为私有，或需要 Hugging Face 账号授权。",
                    429 => "请求频率受限，请稍后重试。",
                    _ => "请稍后重试并检查网络连接。",
                };
                return Err(format!(
                    "{action}失败：Hugging Face 返回 HTTP {status}。{detail}"
                ));
            }
            Err(error) => {
                let retryable = error.is_timeout() || error.is_connect();
                if retryable && attempt < 3 {
                    thread::sleep(Duration::from_millis(350 * u64::from(attempt)));
                    continue;
                }
                let category = if error.is_timeout() {
                    "请求超时"
                } else if error.is_connect() {
                    "无法连接 Hugging Face；请检查网络、DNS 或代理设置"
                } else {
                    "网络请求出错"
                };
                let mut causes = vec![error.to_string()];
                let mut source = std::error::Error::source(&error);
                while let Some(cause) = source {
                    causes.push(cause.to_string());
                    source = cause.source();
                }
                return Err(format!(
                    "{action}失败：{category}（已尝试 {attempt} 次）：{}",
                    causes.join(" → ")
                ));
            }
        }
    }
    Err(format!("{action}失败：网络请求重试次数已用尽。"))
}

pub fn search_huggingface(query: &str) -> Result<Vec<HuggingFaceModel>, String> {
    let query = query.trim();
    if query.len() < 2 || query.len() > 120 {
        return Err("请输入 2 到 120 个字符的模型关键词".to_string());
    }
    let url = format!(
        "https://huggingface.co/api/models?search={}&filter=gguf&sort=downloads&limit=20",
        percent_encode(query)
    );
    let response = hf_get(&url, "查询 Hugging Face")?;
    let models: Vec<ApiModel> = response
        .json()
        .map_err(|error| format!("解析模型搜索结果失败：{error}"))?;
    Ok(models
        .into_iter()
        .filter(|model| is_safe_repo_id(&model.id))
        .map(|model| HuggingFaceModel {
            repo_id: model.id,
            downloads: model.downloads,
            likes: model.likes,
            pipeline_tag: model.pipeline_tag,
        })
        .collect())
}

pub fn huggingface_files(repo_id: &str) -> Result<Vec<HuggingFaceFile>, String> {
    if !is_safe_repo_id(repo_id) {
        return Err("模型仓库标识格式无效".to_string());
    }
    let url = format!(
        "https://huggingface.co/api/models/{}?blobs=true",
        percent_encode(repo_id)
    );
    let response = hf_get(&url, "获取仓库文件")?;
    let model: ApiModelDetails = response
        .json()
        .map_err(|error| format!("解析仓库文件列表失败：{error}"))?;
    let mut files: Vec<HuggingFaceFile> = model
        .siblings
        .into_iter()
        .filter(|file| {
            file.rfilename.to_ascii_lowercase().ends_with(".gguf")
                && is_safe_relative_file(&file.rfilename)
        })
        .map(|file| HuggingFaceFile {
            name: file.rfilename,
            size_bytes: file.size.or_else(|| file.lfs.and_then(|lfs| lfs.size)),
        })
        .collect();
    files.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(files)
}

pub fn download_huggingface_model<F>(
    app: &AppHandle,
    repo_id: &str,
    file_name: &str,
    mut progress: F,
) -> Result<Vec<ModelInfo>, String>
where
    F: FnMut(u64, u64),
{
    if !is_safe_repo_id(repo_id)
        || !is_safe_relative_file(file_name)
        || !file_name.to_ascii_lowercase().ends_with(".gguf")
    {
        return Err("模型仓库或文件名无效；只允许下载仓库中的 GGUF 文件".to_string());
    }
    let encoded_repo = percent_encode(repo_id);
    let encoded_file = percent_encode_path(file_name);
    let url = format!("https://huggingface.co/{encoded_repo}/resolve/main/{encoded_file}");
    let response = hf_get(&url, "下载模型")?;
    let final_url = response.url();
    if final_url.scheme() != "https"
        || !is_huggingface_host(final_url.host_str().unwrap_or_default())
    {
        return Err("模型下载最终地址不属于 Hugging Face 官方 HTTPS 域名".to_string());
    }
    let total = response.content_length().unwrap_or(0);
    if total > MAX_MODEL_DOWNLOAD_BYTES {
        return Err("模型文件超过 200 GB 下载上限".to_string());
    }
    let root = models_directory(app)?;
    let flat_repo = repo_id.replace('/', "--");
    let flat_file = file_name.replace('/', "__");
    let target_name = format!("{flat_repo}__{flat_file}");
    if target_name.len() > 240
        || !target_name
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err("生成的本地模型文件名不安全或过长".to_string());
    }
    let target = root.join(&target_name);
    if target.exists() {
        return Err(format!("模型文件已存在：{target_name}"));
    }
    let partial = root.join(format!(".{target_name}.download"));
    let result = (|| {
        let mut input = response;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .map_err(|error| format!("无法创建模型临时文件：{error}"))?;
        let mut buffer = [0u8; 256 * 1024];
        let mut received = 0u64;
        loop {
            let count = input
                .read(&mut buffer)
                .map_err(|error| format!("模型下载中断：{error}"))?;
            if count == 0 {
                break;
            }
            received = received
                .checked_add(count as u64)
                .ok_or_else(|| "模型文件大小溢出".to_string())?;
            if received > MAX_MODEL_DOWNLOAD_BYTES || (total > 0 && received > total) {
                return Err("下载内容超过声明大小或安全上限".to_string());
            }
            output
                .write_all(&buffer[..count])
                .map_err(|error| format!("写入模型文件失败：{error}"))?;
            progress(received, total);
        }
        if total > 0 && received != total {
            return Err(format!(
                "下载不完整：收到 {received} bytes，预期 {total} bytes"
            ));
        }
        output.flush().map_err(|error| error.to_string())?;
        output.sync_all().map_err(|error| error.to_string())?;
        drop(output);
        fs::rename(&partial, &target).map_err(|error| format!("保存模型失败：{error}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result?;
    list_models(app)
}

fn is_safe_repo_id(value: &str) -> bool {
    let mut parts = value.split('/');
    let Some(owner) = parts.next() else {
        return false;
    };
    let Some(repo) = parts.next() else {
        return false;
    };
    parts.next().is_none() && safe_hf_segment(owner) && safe_hf_segment(repo)
}

fn is_safe_relative_file(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains('\\')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && !part.contains(':'))
        && value.chars().all(|ch| !ch.is_control())
}

fn safe_hf_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn percent_encode_path(value: &str) -> String {
    value
        .split('/')
        .map(percent_encode)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_and_file_validation_blocks_path_traversal() {
        assert!(is_safe_repo_id("Qwen/Qwen3-4B-GGUF"));
        assert!(!is_safe_repo_id("../evil"));
        assert!(!is_safe_repo_id("org/model/branch"));
        assert!(is_safe_relative_file("Q4_K_M/model.gguf"));
        assert!(!is_safe_relative_file("../model.gguf"));
        assert!(!is_safe_relative_file("folder\\model.gguf"));
    }

    #[test]
    fn hug_face_redirect_allowlist_is_host_bound() {
        assert!(is_huggingface_host("huggingface.co"));
        assert!(is_huggingface_host("cdn-lfs.huggingface.co"));
        assert!(is_huggingface_host("cas-bridge.xethub.hf.co"));
        assert!(!is_huggingface_host("huggingface.co.attacker.example"));
        assert!(!is_huggingface_host("attacker.hf.co.evil"));
    }

    #[test]
    fn path_encoding_keeps_slashes_but_encodes_spaces() {
        assert_eq!(
            percent_encode_path("Q4 K M/model.gguf"),
            "Q4%20K%20M/model.gguf"
        );
    }
}
