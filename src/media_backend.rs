use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::AppHandle;

const MAX_MEDIA_FILES_PER_BUNDLE: usize = 32;
const MAX_MEDIA_BUNDLE_BYTES: u64 = 200 * 1024 * 1024 * 1024;
const MIN_FREE_RESERVE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_MEDIA_FILE_BYTES: u64 = 150 * 1024 * 1024 * 1024;
const MAX_PROMPT_BYTES: usize = 8 * 1024;
const MAX_IMAGE_PREVIEW_BYTES: u64 = 32 * 1024 * 1024;
const MAX_IMAGE_INDEX_ENTRIES: usize = 20_000;
const MAX_IMAGE_INDEX_RESULTS: usize = 100;
const STDERR_TAIL_BYTES: usize = 48 * 1024;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
static OUTPUT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaModelFile {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
    pub format: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteMediaFile {
    pub name: String,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
    pub license: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiModelDetails {
    sha: String,
    #[serde(default)]
    siblings: Vec<ApiSibling>,
    #[serde(default)]
    card_data: Option<ApiCardData>,
}

#[derive(Debug, Deserialize)]
struct ApiCardData {
    license: Option<String>,
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
    sha256: Option<String>,
    oid: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaGenerationRequest {
    pub kind: String,
    pub diffusion_model_path: String,
    pub vae_path: Option<String>,
    pub text_encoder_path: Option<String>,
    #[serde(default)]
    pub text_encoder_mode: Option<String>,
    #[serde(default)]
    pub cfg_scale: Option<f32>,
    pub high_noise_model_path: Option<String>,
    pub prompt: String,
    pub negative_prompt: String,
    pub width: u32,
    pub height: u32,
    pub steps: u32,
    pub video_frames: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaGenerationResult {
    pub output_path: String,
    pub kind: String,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneratedImage {
    pub path: String,
    pub name: String,
    pub size_bytes: u64,
    pub modified_at_ms: u64,
}

pub fn list_generated_images(app: &AppHandle) -> Result<Vec<GeneratedImage>, String> {
    let root = crate::desktop_backend::creations_directory(app, "image")?;
    let entries = fs::read_dir(&root).map_err(|error| format!("读取作品目录失败：{error}"))?;
    let mut images = Vec::new();
    for entry in entries.take(MAX_IMAGE_INDEX_ENTRIES) {
        let entry = entry.map_err(|error| format!("读取作品目录条目失败：{error}"))?;
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if !file_type.is_file()
            || file_type.is_symlink()
            || !entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
        {
            continue;
        }
        let path = entry
            .path()
            .canonicalize()
            .map_err(|error| format!("无法确认作品路径：{error}"))?;
        if path.parent() != Some(root.as_path()) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let modified_at_ms = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        images.push(GeneratedImage {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: path.display().to_string(),
            size_bytes: metadata.len(),
            modified_at_ms,
        });
    }
    images.sort_by(|left, right| {
        right
            .modified_at_ms
            .cmp(&left.modified_at_ms)
            .then_with(|| left.name.cmp(&right.name))
    });
    images.truncate(MAX_IMAGE_INDEX_RESULTS);
    Ok(images)
}

pub fn preview_generated_image(app: &AppHandle, image_path: &str) -> Result<String, String> {
    if image_path.is_empty() || image_path.len() > 16 * 1024 {
        return Err("作品路径无效".to_string());
    }
    let supplied_path = PathBuf::from(image_path);
    let metadata = fs::symlink_metadata(&supplied_path)
        .map_err(|error| format!("无法读取作品文件：{error}"))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err("作品预览只接受图片目录中的普通文件".to_string());
    }
    if metadata.len() == 0 || metadata.len() > MAX_IMAGE_PREVIEW_BYTES {
        return Err("图片为空或超过 32 MiB 的预览上限；可在作品文件夹中打开".to_string());
    }
    let path = supplied_path
        .canonicalize()
        .map_err(|error| format!("无法确认作品路径：{error}"))?;
    let root = crate::desktop_backend::creations_directory(app, "image")?;
    if path.parent() != Some(root.as_path())
        || !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
    {
        return Err("只允许预览当前作品目录中的 PNG 图片".to_string());
    }
    let bytes = fs::read(&path).map_err(|error| format!("读取图片失败：{error}"))?;
    if !bytes.starts_with(PNG_SIGNATURE) {
        return Err("文件扩展名为 PNG，但内容不是有效的 PNG 图片".to_string());
    }
    Ok(format!("data:image/png;base64,{}", base64_encode(&bytes)))
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().saturating_add(2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = chunk.get(1).copied().map(u32::from).unwrap_or(0);
        let third = chunk.get(2).copied().map(u32::from).unwrap_or(0);
        let value = (first << 16) | (second << 8) | third;
        encoded.push(ALPHABET[((value >> 18) & 0x3f) as usize] as char);
        encoded.push(ALPHABET[((value >> 12) & 0x3f) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((value >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(value & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    encoded
}

pub struct GenerationControl {
    cancelled: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl GenerationControl {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            child: Mutex::new(None),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Ok(mut child) = self.child.lock() {
            if let Some(child) = child.as_mut() {
                let _ = child.kill();
            }
        }
    }
}

pub fn media_models_directory(app: &AppHandle) -> Result<PathBuf, String> {
    let directory = crate::desktop_backend::models_directory(app)?.join("media");
    fs::create_dir_all(&directory).map_err(|error| format!("无法创建媒体模型目录：{error}"))?;
    directory
        .canonicalize()
        .map_err(|error| format!("无法确认媒体模型目录：{error}"))
}

pub fn list_media_models(app: &AppHandle) -> Result<Vec<MediaModelFile>, String> {
    let root = media_models_directory(app)?;
    let entries = fs::read_dir(&root).map_err(|error| format!("读取媒体模型目录失败：{error}"))?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if !file_type.is_file()
            || file_type.is_symlink()
            || !is_supported_media_weight(&entry.path())
        {
            continue;
        }
        let path = entry
            .path()
            .canonicalize()
            .map_err(|error| error.to_string())?;
        if path.parent() != Some(root.as_path()) {
            continue;
        }
        let metadata = fs::metadata(&path).map_err(|error| error.to_string())?;
        files.push(MediaModelFile {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: path.display().to_string(),
            size_bytes: metadata.len(),
            format: path
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase(),
        });
    }
    files.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()));
    Ok(files)
}

pub fn huggingface_media_files(repo_id: &str) -> Result<Vec<RemoteMediaFile>, String> {
    if !safe_repo_id(repo_id) {
        return Err("模型仓库标识格式无效".to_string());
    }
    let client = blocking_hf_client()?;
    let url = format!(
        "https://huggingface.co/api/models/{}?blobs=true",
        encode(repo_id)
    );
    let response = client
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| format!("获取媒体模型文件列表失败：{error}"))?;
    let details: ApiModelDetails = response
        .json()
        .map_err(|error| format!("解析 Hugging Face 文件列表失败：{error}"))?;
    if !safe_revision(&details.sha) {
        return Err("Hugging Face 返回的版本标识不安全；拒绝列出下载项".to_string());
    }
    let license = details.card_data.and_then(|card| card.license);
    let mut files: Vec<RemoteMediaFile> = details
        .siblings
        .into_iter()
        .filter(|file| {
            safe_relative_file(&file.rfilename)
                && is_supported_media_weight(Path::new(&file.rfilename))
        })
        .map(|file| {
            let size_bytes = file
                .size
                .or_else(|| file.lfs.as_ref().and_then(|lfs| lfs.size));
            let sha256 = file.lfs.and_then(|lfs| {
                lfs.sha256.or_else(|| {
                    lfs.oid
                        .and_then(|oid| oid.strip_prefix("sha256:").map(str::to_string))
                })
            });
            RemoteMediaFile {
                name: file.rfilename,
                size_bytes,
                sha256,
                license: license.clone(),
            }
        })
        .collect();
    files.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(files)
}

fn blocking_hf_client() -> Result<reqwest::blocking::Client, String> {
    let allow = |attempt: reqwest::redirect::Attempt<'_>| {
        let url = attempt.url();
        if url.scheme() == "https" && is_hf_host(url.host_str().unwrap_or_default()) {
            attempt.follow()
        } else {
            attempt.error("拒绝跳转到非 Hugging Face 官方 HTTPS 域名")
        }
    };
    reqwest::blocking::Client::builder()
        .user_agent(concat!("local-engine-manager/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(60))
        .redirect(Policy::custom(allow))
        .build()
        .map_err(|error| error.to_string())
}

fn async_hf_client() -> Result<reqwest::Client, String> {
    let allow = |attempt: reqwest::redirect::Attempt<'_>| {
        let url = attempt.url();
        if url.scheme() == "https" && is_hf_host(url.host_str().unwrap_or_default()) {
            attempt.follow()
        } else {
            attempt.error("拒绝跳转到非 Hugging Face 官方 HTTPS 域名")
        }
    };
    reqwest::Client::builder()
        .user_agent(concat!("local-engine-manager/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(60 * 60 * 3))
        .redirect(Policy::custom(allow))
        .build()
        .map_err(|error| error.to_string())
}

fn is_hf_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("huggingface.co")
        || host.to_ascii_lowercase().ends_with(".huggingface.co")
        || host.eq_ignore_ascii_case("hf.co")
        || host.to_ascii_lowercase().ends_with(".hf.co")
}

async fn wait_for_cancel(cancelled: &AtomicBool) {
    while !cancelled.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn hf_get(
    client: &reqwest::Client,
    url: &str,
    cancelled: &AtomicBool,
) -> Result<reqwest::Response, String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("下载已取消".to_string());
    }
    let response = tokio::select! {
        biased;
        _ = wait_for_cancel(cancelled) => return Err("下载已取消".to_string()),
        result = client.get(url).send() => result.map_err(|error| format!("连接 Hugging Face 失败：{error}"))?,
    };
    if !response.status().is_success() {
        return Err(format!(
            "Hugging Face 返回 HTTP {}；仓库可能不存在、需要授权或暂时限流",
            response.status()
        ));
    }
    let final_url = response.url();
    if final_url.scheme() != "https" || !is_hf_host(final_url.host_str().unwrap_or_default()) {
        return Err("下载最终地址不属于 Hugging Face 官方 HTTPS 域名".to_string());
    }
    Ok(response)
}

pub async fn download_media_bundle<F>(
    app: &AppHandle,
    repo_id: &str,
    file_names: &[String],
    cancelled: &AtomicBool,
    mut progress: F,
) -> Result<Vec<MediaModelFile>, String>
where
    F: FnMut(&str, u64, u64, u64, u64) + Send + 'static,
{
    if !safe_repo_id(repo_id) {
        return Err("模型仓库标识格式无效".to_string());
    }
    if file_names.is_empty() || file_names.len() > MAX_MEDIA_FILES_PER_BUNDLE {
        return Err(format!(
            "每个媒体下载组必须包含 1 到 {MAX_MEDIA_FILES_PER_BUNDLE} 个文件"
        ));
    }
    let mut unique = HashSet::new();
    if file_names
        .iter()
        .any(|name| !safe_relative_file(name) || !unique.insert(name.clone()))
    {
        return Err("文件清单含重复项或不安全路径".to_string());
    }
    if cancelled.load(Ordering::Relaxed) {
        return Err("下载已取消".to_string());
    }
    let client = async_hf_client()?;
    let details_url = format!(
        "https://huggingface.co/api/models/{}?blobs=true",
        encode(repo_id)
    );
    let details_response = hf_get(&client, &details_url, cancelled).await?;
    let details: ApiModelDetails = tokio::select! {
        biased;
        _ = wait_for_cancel(cancelled) => return Err("下载已取消".to_string()),
        result = details_response.json() => result.map_err(|error| format!("解析 Hugging Face 文件列表失败：{error}"))?,
    };
    if !safe_revision(&details.sha) {
        return Err("Hugging Face 返回的版本标识不安全；拒绝下载".to_string());
    }
    let revision = details.sha;
    let mut metadata_by_name = HashMap::new();
    for sibling in details.siblings {
        if safe_relative_file(&sibling.rfilename)
            && is_supported_media_weight(Path::new(&sibling.rfilename))
        {
            let size_bytes = sibling
                .size
                .or_else(|| sibling.lfs.as_ref().and_then(|lfs| lfs.size));
            let sha256 = sibling.lfs.and_then(|lfs| {
                lfs.sha256.or_else(|| {
                    lfs.oid
                        .and_then(|oid| oid.strip_prefix("sha256:").map(str::to_string))
                })
            });
            metadata_by_name.insert(sibling.rfilename, (size_bytes, sha256));
        }
    }
    let mut chosen = Vec::with_capacity(file_names.len());
    let mut total_bytes = 0u64;
    for name in file_names {
        let (size, hash) = metadata_by_name.get(name).ok_or_else(|| {
            format!("所选文件不在该仓库本次固定版本的 Safetensors/GGUF 清单中：{name}")
        })?;
        let size = size.ok_or_else(|| format!("仓库未提供可核实文件大小，已阻止下载：{name}"))?;
        let hash = hash
            .clone()
            .filter(|value| is_sha256(value))
            .ok_or_else(|| format!("仓库未提供可核实 LFS SHA-256，已阻止下载：{name}"))?;
        if size == 0 || size > MAX_MEDIA_FILE_BYTES {
            return Err(format!("媒体组件大小无效或超过 150 GiB 安全上限：{name}"));
        }
        total_bytes = total_bytes
            .checked_add(size)
            .ok_or_else(|| "媒体下载组总大小溢出".to_string())?;
        chosen.push((name.clone(), size, hash));
    }
    if total_bytes > MAX_MEDIA_BUNDLE_BYTES {
        return Err("媒体组件组超过 200 GiB 总下载上限".to_string());
    }
    let root = media_models_directory(app)?;
    let mut pending_bytes = 0u64;
    for (name, size, hash) in &chosen {
        let target = root.join(media_target_name(repo_id, name)?);
        if !existing_media_target_matches(&target, hash)? {
            pending_bytes = pending_bytes
                .checked_add(*size)
                .ok_or_else(|| "待下载媒体组件总大小溢出".to_string())?;
        }
    }
    let storage = crate::desktop_backend::model_storage_snapshot(app)?;
    let available = storage.available_bytes.ok_or_else(|| {
        "无法检测模型目录所在磁盘可用空间；为避免写满磁盘，已阻止下载".to_string()
    })?;
    let required = if pending_bytes == 0 {
        0
    } else {
        pending_bytes
            .checked_add(MIN_FREE_RESERVE_BYTES)
            .ok_or_else(|| "磁盘空间需求溢出".to_string())?
    };
    if available < required {
        return Err(format!(
            "磁盘空间不足：尚待下载组件共 {}，另需预留 512 MiB；当前剩余 {}",
            pending_bytes, available
        ));
    }
    if cancelled.load(Ordering::Relaxed) {
        return Err("下载已取消".to_string());
    }
    let mut total_complete = 0u64;
    let mut failures = Vec::new();
    for (name, size, hash) in chosen {
        if cancelled.load(Ordering::Relaxed) {
            return Err(format!(
                "下载已取消；先前已校验组件保留在媒体模型目录中：{}",
                root.display()
            ));
        }
        let target_name = media_target_name(repo_id, &name)?;
        let target = root.join(&target_name);
        if existing_media_target_matches(&target, &hash)? {
            total_complete = total_complete.saturating_add(size);
            progress(&name, size, size, total_complete, total_bytes);
            continue;
        }
        let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let partial = root.join(format!(
            ".{target_name}.{}.{}.part",
            std::process::id(),
            nonce
        ));
        let encoded_repo = encode(repo_id);
        let encoded_file = encode_path(&name);
        let url =
            format!("https://huggingface.co/{encoded_repo}/resolve/{revision}/{encoded_file}");
        let mut response = match hf_get(&client, &url, cancelled).await {
            Ok(response) => response,
            Err(error) => {
                failures.push(format!("{name}: {error}"));
                break;
            }
        };
        if response
            .content_length()
            .is_some_and(|length| length != size)
        {
            failures.push(format!("{name}: 下载服务器声明大小与仓库元数据不一致"));
            break;
        }
        let mut output = match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .await
        {
            Ok(file) => file,
            Err(error) => return Err(format!("无法创建媒体组件临时文件：{error}")),
        };
        let mut received = 0u64;
        let mut hasher = Sha256::new();
        let transfer: Result<(), String> = async {
            loop {
                let chunk = tokio::select! {
                    biased;
                    _ = wait_for_cancel(cancelled) => return Err("下载已取消".to_string()),
                    result = response.chunk() => result.map_err(|error| format!("下载中断：{error}"))?,
                };
                let Some(chunk) = chunk else { break };
                if cancelled.load(Ordering::Relaxed) { return Err("下载已取消".to_string()); }
                received = received.checked_add(chunk.len() as u64).ok_or_else(|| "下载大小溢出".to_string())?;
                if received > size || received > MAX_MEDIA_FILE_BYTES {
                    return Err("下载内容超过仓库声明大小或安全上限".to_string());
                }
                use tokio::io::AsyncWriteExt;
                output.write_all(&chunk).await.map_err(|error| format!("写入临时文件失败：{error}"))?;
                hasher.update(&chunk);
                progress(&name, received, size, total_complete.saturating_add(received), total_bytes);
            }
            if received != size { return Err(format!("下载不完整：收到 {received} bytes，预期 {size} bytes")); }
            let actual = format!("{:x}", hasher.finalize());
            if !actual.eq_ignore_ascii_case(&hash) { return Err("SHA-256 校验失败".to_string()); }
            use tokio::io::AsyncWriteExt;
            output.flush().await.map_err(|error| error.to_string())?;
            output.sync_all().await.map_err(|error| error.to_string())?;
            drop(output);
            if cancelled.load(Ordering::Relaxed) { return Err("下载已取消".to_string()); }
            Ok(())
        }.await;
        if let Err(error) = transfer {
            let cleanup = match tokio::fs::remove_file(&partial).await {
                Ok(()) => String::new(),
                Err(clean_error) if clean_error.kind() == std::io::ErrorKind::NotFound => {
                    String::new()
                }
                Err(clean_error) => format!(
                    "；临时文件清理失败，请检查 {}：{clean_error}",
                    partial.display()
                ),
            };
            failures.push(format!("{name}: {error}{cleanup}"));
            break;
        }
        if let Err(error) = commit_no_replace(&partial, &target) {
            let _ = tokio::fs::remove_file(&partial).await;
            failures.push(format!("{name}: 安全提交失败（不会覆盖已有文件）：{error}"));
            break;
        }
        total_complete = total_complete.saturating_add(size);
        progress(&name, size, size, total_complete, total_bytes);
    }
    if !failures.is_empty() {
        let completed_names = file_names
            .iter()
            .filter(|name| {
                media_target_name(repo_id, name)
                    .ok()
                    .is_some_and(|target| root.join(target).is_file())
            })
            .cloned()
            .collect::<Vec<_>>();
        let preserved = if completed_names.is_empty() {
            String::new()
        } else {
            format!(
                "；已校验组件保留，可重试续传：{}",
                completed_names.join("、")
            )
        };
        let cancelled_text = if cancelled.load(Ordering::Relaxed) {
            "下载已取消；"
        } else {
            "媒体模型组件下载未全部完成；"
        };
        return Err(format!(
            "{cancelled_text}{}{preserved}",
            failures.join("；")
        ));
    }
    list_media_models(app)
}

fn media_target_name(repo_id: &str, file_name: &str) -> Result<String, String> {
    if !safe_repo_id(repo_id) || !safe_relative_file(file_name) {
        return Err("模型仓库或文件名无效".to_string());
    }
    let repo = repo_id.replace('/', "--");
    let file = file_name.replace('/', "__");
    let name = format!("{repo}__{file}");
    if name.len() > 240
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err("生成的本地媒体模型文件名不安全或过长".to_string());
    }
    Ok(name)
}

fn commit_no_replace(partial: &Path, target: &Path) -> std::io::Result<()> {
    fs::hard_link(partial, target)?;
    fs::remove_file(partial)
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn existing_media_target_matches(path: &Path, expected_hash: &str) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("检查已存在组件失败：{error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(format!(
            "媒体模型目标不是普通文件；拒绝读取或覆盖：{}",
            path.display()
        ));
    }
    let actual = sha256_file(path).map_err(|error| format!("检查已存在组件失败：{error}"))?;
    if actual.eq_ignore_ascii_case(expected_hash) {
        Ok(true)
    } else {
        Err(format!(
            "媒体模型目录已有同名但哈希不同的文件；未覆盖：{}",
            path.display()
        ))
    }
}

fn safe_repo_id(value: &str) -> bool {
    let mut parts = value.split('/');
    let Some(owner) = parts.next() else {
        return false;
    };
    let Some(repo) = parts.next() else {
        return false;
    };
    parts.next().is_none() && safe_segment(owner) && safe_segment(repo)
}

fn safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn safe_relative_file(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains('\\')
        && value.split('/').all(safe_segment)
        && value.chars().all(|ch| !ch.is_control())
}

fn is_supported_media_weight(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            ext.eq_ignore_ascii_case("safetensors") || ext.eq_ignore_ascii_case("gguf")
        })
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn safe_revision(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn encode(value: &str) -> String {
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

fn encode_path(value: &str) -> String {
    value.split('/').map(encode).collect::<Vec<_>>().join("/")
}

fn validate_generation_request(request: &MediaGenerationRequest) -> Result<(), String> {
    if !matches!(request.kind.as_str(), "image" | "video") {
        return Err("创作类型无效".to_string());
    }
    if request.prompt.trim().is_empty()
        || request.prompt.len() > MAX_PROMPT_BYTES
        || request.negative_prompt.len() > MAX_PROMPT_BYTES
    {
        return Err("提示词不能为空，且正向/反向提示词最多各 8 KiB".to_string());
    }
    if !(256..=1024).contains(&request.width)
        || !(256..=1024).contains(&request.height)
        || request.width % 16 != 0
        || request.height % 16 != 0
    {
        return Err(
            "宽高必须是 256 至 1024 间的 16 的倍数；更高分辨率暂不开放以降低内存风险".to_string(),
        );
    }
    if !(1..=100).contains(&request.steps) {
        return Err("生成步数必须在 1 到 100 之间".to_string());
    }
    if request.kind == "video" && request.video_frames.unwrap_or(33) != 33 {
        return Err("当前视频模式固定生成 33 帧；更长片段会显著增加资源需求".to_string());
    }
    if request
        .text_encoder_mode
        .as_deref()
        .is_some_and(|mode| !matches!(mode, "t5xxl" | "llm"))
    {
        return Err("文本编码器类型无效；仅支持官方 CLI 的 t5xxl 或 llm 参数".to_string());
    }
    if request
        .cfg_scale
        .is_some_and(|scale| !scale.is_finite() || !(0.0..=30.0).contains(&scale))
    {
        return Err("CFG scale 必须是 0 到 30 之间的有限数值".to_string());
    }
    Ok(())
}

fn allowed_weight_path(app: &AppHandle, input: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(input)
        .canonicalize()
        .map_err(|error| format!("无法定位模型组件：{error}"))?;
    if !path.is_file() || !is_supported_media_weight(&path) {
        return Err("媒体模型只接受实际文件形式的 .safetensors 或 .gguf 组件".to_string());
    }
    let managed = media_models_directory(app)?;
    if path.starts_with(&managed) {
        return Ok(path);
    }
    let settings = crate::desktop_backend::load_settings(app)?;
    let external = settings
        .comfyui_directory
        .ok_or_else(|| "外部模型路径不在已选择的 ComfyUI 文件夹内".to_string())?;
    let scan = crate::desktop_backend::scan_comfyui_models(&external)?;
    let root = PathBuf::from(scan.models_directory)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if path.starts_with(&root) {
        Ok(path)
    } else {
        Err("模型组件不在媒体模型目录或已选择的 ComfyUI 文件夹内；拒绝读取该路径".to_string())
    }
}

fn locate_sd_cli() -> Result<PathBuf, String> {
    let statuses = crate::engine_manager::desktop_statuses().map_err(|error| error.to_string())?;
    let status = statuses
        .iter()
        .find(|item| item.engine == "stable-diffusion.cpp")
        .ok_or_else(|| "无法读取 stable-diffusion.cpp 安装状态".to_string())?;
    if status.installed_versions.is_empty() {
        return Err("尚未安装 stable-diffusion.cpp；请先安装官方 CPU x64 引擎".to_string());
    }
    for version in status.installed_versions.iter().rev() {
        let root = PathBuf::from(&status.install_path).join(&version.tag);
        if let Some(path) = find_named_file(
            &root,
            if cfg!(windows) {
                "sd-cli.exe"
            } else {
                "sd-cli"
            },
            5,
        ) {
            return Ok(path);
        }
    }
    Err("已安装的 stable-diffusion.cpp 中未找到 sd-cli 可执行文件".to_string())
}

fn find_named_file(root: &Path, name: &str, max_depth: usize) -> Option<PathBuf> {
    fn walk(path: &Path, name: &str, depth: usize, max_depth: usize) -> Option<PathBuf> {
        if depth > max_depth {
            return None;
        }
        for entry in fs::read_dir(path).ok()?.flatten() {
            let kind = entry.file_type().ok()?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_file()
                && entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(name)
            {
                return Some(entry.path());
            }
            if kind.is_dir() {
                if let Some(found) = walk(&entry.path(), name, depth + 1, max_depth) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(root, name, 0, max_depth)
}

fn output_path(app: &AppHandle, kind: &str) -> Result<PathBuf, String> {
    let directory = crate::desktop_backend::creations_directory(app, kind)?;
    for _ in 0..4 {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let count = OUTPUT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = if kind == "image" {
            format!("image-{stamp}-{count}.png")
        } else {
            format!("video-{stamp}-{count}.webm")
        };
        let path = directory.join(name);
        if !path.exists() {
            return Ok(path);
        }
    }
    Err("无法为作品分配一个不覆盖现有文件的文件名".to_string())
}

pub fn generate_media(
    app: &AppHandle,
    request: MediaGenerationRequest,
    control: Arc<GenerationControl>,
) -> Result<MediaGenerationResult, String> {
    validate_generation_request(&request)?;
    let model = allowed_weight_path(app, &request.diffusion_model_path)?;
    let vae = request
        .vae_path
        .as_deref()
        .map(|path| allowed_weight_path(app, path))
        .transpose()?;
    let text_encoder = request
        .text_encoder_path
        .as_deref()
        .map(|path| allowed_weight_path(app, path))
        .transpose()?;
    let high_noise = request
        .high_noise_model_path
        .as_deref()
        .map(|path| allowed_weight_path(app, path))
        .transpose()?;
    if request.kind == "video" && (vae.is_none() || text_encoder.is_none()) {
        return Err(
            "Wan 文生视频需要选择 VAE 与 T5-XXL 文本编码器；Wan2.2 双专家还需高噪声模型"
                .to_string(),
        );
    }
    let executable = locate_sd_cli()?;
    let output = output_path(app, &request.kind)?;
    let mut args: Vec<String> = Vec::new();
    if request.kind == "image" {
        args.extend(["--diffusion-model".to_string(), model.display().to_string()]);
        if let Some(vae) = vae {
            args.extend(["--vae".into(), vae.display().to_string()]);
        }
        if let Some(text_encoder) = text_encoder {
            let text_flag = if request.text_encoder_mode.as_deref() == Some("llm") {
                "--llm"
            } else {
                "--t5xxl"
            };
            args.extend([text_flag.into(), text_encoder.display().to_string()]);
        }
    } else {
        args.extend([
            "-M".into(),
            "vid_gen".into(),
            "--diffusion-model".into(),
            model.display().to_string(),
        ]);
        if let Some(high_noise) = high_noise {
            args.extend([
                "--high-noise-diffusion-model".into(),
                high_noise.display().to_string(),
            ]);
        }
        args.extend([
            "--vae".into(),
            vae.expect("validated video VAE").display().to_string(),
        ]);
        args.extend([
            "--t5xxl".into(),
            text_encoder
                .expect("validated video T5")
                .display()
                .to_string(),
        ]);
        args.extend([
            "--video-frames".into(),
            "33".into(),
            "--flow-shift".into(),
            "3.0".into(),
        ]);
    }
    args.extend([
        "--cfg-scale".into(),
        format!("{:.3}", request.cfg_scale.unwrap_or(6.0)),
    ]);
    args.extend(["--sampling-method".into(), "euler".into()]);
    args.extend(["-p".into(), request.prompt]);
    if !request.negative_prompt.trim().is_empty() {
        args.extend(["-n".into(), request.negative_prompt]);
    }
    args.extend([
        "-W".into(),
        request.width.to_string(),
        "-H".into(),
        request.height.to_string(),
        "--steps".into(),
        request.steps.to_string(),
        "-o".into(),
        output.display().to_string(),
    ]);

    let mut command = Command::new(&executable);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("启动官方 sd-cli 失败：{error}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法读取 sd-cli 错误输出".to_string())?;
    let tail = Arc::new(Mutex::new(VecDeque::<u8>::with_capacity(STDERR_TAIL_BYTES)));
    let reader_tail = tail.clone();
    let reader = thread::spawn(move || {
        let mut stderr = stderr;
        let mut buffer = [0u8; 4096];
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    if let Ok(mut tail) = reader_tail.lock() {
                        for byte in &buffer[..count] {
                            if tail.len() == STDERR_TAIL_BYTES {
                                tail.pop_front();
                            }
                            tail.push_back(*byte);
                        }
                    }
                }
            }
        }
    });
    {
        let mut slot = control
            .child
            .lock()
            .map_err(|_| "无法记录创作引擎进程".to_string())?;
        *slot = Some(child);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(6 * 60 * 60);
    let status = loop {
        if control.cancelled.load(Ordering::Relaxed) {
            if let Ok(mut slot) = control.child.lock() {
                if let Some(child) = slot.as_mut() {
                    let _ = child.kill();
                }
            }
        }
        let checked = {
            let mut slot = control
                .child
                .lock()
                .map_err(|_| "无法检查创作引擎进程".to_string())?;
            slot.as_mut()
                .ok_or_else(|| "创作引擎进程已丢失".to_string())?
                .try_wait()
        };
        match checked {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return Err(format!("检查创作引擎进程失败：{error}")),
        }
        if std::time::Instant::now() >= deadline {
            control.cancel();
            break {
                let mut slot = control
                    .child
                    .lock()
                    .map_err(|_| "无法停止超时创作进程".to_string())?;
                let child = slot
                    .as_mut()
                    .ok_or_else(|| "创作进程状态丢失".to_string())?;
                let _ = child.kill();
                child
                    .wait()
                    .map_err(|error| format!("等待超时创作进程退出失败：{error}"))?
            };
        }
        thread::sleep(Duration::from_millis(200));
    };
    {
        let mut slot = control
            .child
            .lock()
            .map_err(|_| "无法清理创作引擎进程记录".to_string())?;
        if let Some(mut child) = slot.take() {
            let _ = child.wait();
        }
    }
    let _ = reader.join();
    let stderr_text = tail
        .lock()
        .map(|bytes| {
            String::from_utf8_lossy(&bytes.iter().copied().collect::<Vec<_>>())
                .trim()
                .to_string()
        })
        .unwrap_or_default();
    if control.cancelled.load(Ordering::Relaxed) {
        let _ = fs::remove_file(&output);
        return Err("创作已取消；本次未完成的临时作品已清理".to_string());
    }
    if !status.success() {
        let _ = fs::remove_file(&output);
        let detail = if stderr_text.is_empty() {
            "引擎未提供诊断输出".to_string()
        } else {
            stderr_text
                .chars()
                .rev()
                .take(4000)
                .collect::<String>()
                .chars()
                .rev()
                .collect()
        };
        return Err(format!(
            "stable-diffusion.cpp 退出状态 {status}。模型/组件组合或系统资源可能不适用。\n{detail}"
        ));
    }
    let metadata = fs::symlink_metadata(&output)
        .map_err(|error| format!("引擎报告成功，但未生成预期作品文件：{error}"))?;
    if !metadata.file_type().is_file() || metadata.len() == 0 {
        let _ = fs::remove_file(&output);
        return Err("引擎未生成有效的非空作品文件".to_string());
    }
    if request.kind == "image" {
        let mut file = File::open(&output).map_err(|error| format!("读取生成图片失败：{error}"))?;
        let mut signature = [0u8; PNG_SIGNATURE.len()];
        if file.read_exact(&mut signature).is_err() || &signature != PNG_SIGNATURE {
            let _ = fs::remove_file(&output);
            return Err("引擎未生成有效的 PNG 图片；无效输出已清理".to_string());
        }
    }
    Ok(MediaGenerationResult {
        output_path: output.display().to_string(),
        kind: request.kind,
    })
}

pub fn cancel_generation(control: &GenerationControl) {
    control.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_extension_filter_allows_only_safetensors_and_gguf() {
        assert!(is_supported_media_weight(Path::new("qwen.safetensors")));
        assert!(is_supported_media_weight(Path::new("text.gguf")));
        assert!(!is_supported_media_weight(Path::new("model.ckpt")));
        assert!(!is_supported_media_weight(Path::new("custom_node.py")));
        assert!(!is_supported_media_weight(Path::new("tokenizer.json")));
    }

    #[test]
    fn base64_encoding_handles_padding_and_empty_input() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }

    #[test]
    fn rejects_path_traversal_and_untrusted_hf_revisions() {
        assert!(safe_repo_id("owner/model-v1"));
        assert!(!safe_repo_id("../model"));
        assert!(safe_relative_file("diffusion_models/wan-Q4.gguf"));
        assert!(!safe_relative_file("../outside.safetensors"));
        assert!(!safe_relative_file("folder\\model.gguf"));
        assert!(safe_revision("0123456789abcdef"));
        assert!(!safe_revision("../../evil"));
    }

    #[test]
    fn only_accepts_sane_media_generation_ranges() {
        let request = MediaGenerationRequest {
            kind: "image".into(),
            diffusion_model_path: "unused".into(),
            vae_path: None,
            text_encoder_path: None,
            text_encoder_mode: None,
            cfg_scale: None,
            high_noise_model_path: None,
            prompt: "cat".into(),
            negative_prompt: String::new(),
            width: 512,
            height: 512,
            steps: 20,
            video_frames: None,
        };
        assert!(validate_generation_request(&request).is_ok());
        let mut qwen = request.clone();
        qwen.text_encoder_mode = Some("llm".into());
        assert!(validate_generation_request(&qwen).is_ok());
        qwen.text_encoder_mode = Some("--run-script".into());
        assert!(validate_generation_request(&qwen).is_err());
        let mut invalid_cfg = request.clone();
        invalid_cfg.cfg_scale = Some(f32::NAN);
        assert!(validate_generation_request(&invalid_cfg).is_err());
        let mut video = request.clone();
        video.kind = "video".into();
        video.video_frames = Some(129);
        assert!(validate_generation_request(&video).is_err());
        let mut huge = request.clone();
        huge.width = 4096;
        assert!(validate_generation_request(&huge).is_err());
    }

    #[test]
    fn target_names_flatten_repo_paths_without_escape() {
        assert_eq!(
            media_target_name("org/model", "vae/wan.safetensors").unwrap(),
            "org--model__vae__wan.safetensors"
        );
        assert!(media_target_name("org/model", "../bad.safetensors").is_err());
    }

    #[test]
    fn existing_media_target_is_only_reused_when_hash_matches() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("model.safetensors");
        fs::write(&path, b"verified model bytes").unwrap();
        let hash = sha256_file(&path).unwrap();
        assert!(existing_media_target_matches(&path, &hash).unwrap());
        assert!(existing_media_target_matches(&path, &"0".repeat(64)).is_err());
        assert!(
            !existing_media_target_matches(&directory.path().join("missing.gguf"), &hash).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_media_target_rejects_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("outside.gguf");
        let link = directory.path().join("model.gguf");
        fs::write(&outside, b"outside file").unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(existing_media_target_matches(&link, &"0".repeat(64)).is_err());
    }
}
