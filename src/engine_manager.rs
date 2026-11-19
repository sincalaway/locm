use flate2::read::GzDecoder;
use reqwest::{
    blocking::{Client, Response},
    redirect::Policy,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(any(not(feature = "desktop"), test))]
use std::ffi::OsString;
use std::{
    env,
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};
use tar::Archive;
use zip::ZipArchive;

const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_SINGLE_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 20_000;
const DOWNLOAD_ATTEMPTS: u8 = 3;
const INSTALL_MANIFEST: &str = "locm-engine-manifest.json";
static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineKind {
    Chat,
    Media,
}

impl EngineKind {
    fn parse(value: &str) -> AppResult<Self> {
        match value {
            "chat" | "llama" | "llama.cpp" => Ok(Self::Chat),
            "media" | "image-video" | "stable-diffusion.cpp" => Ok(Self::Media),
            _ => Err(invalid(format!(
                "未知引擎 {value:?}；可选 chat（llama.cpp）或 media（stable-diffusion.cpp）"
            ))),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Chat => "llama.cpp",
            Self::Media => "stable-diffusion.cpp",
        }
    }

    fn api_repo(self) -> &'static str {
        match self {
            Self::Chat => "ggml-org/llama.cpp",
            Self::Media => "leejet/stable-diffusion.cpp",
        }
    }

    fn asset_suffix(self) -> &'static str {
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            return match self {
                Self::Chat => "-bin-ubuntu-x64.tar.gz",
                Self::Media => "-bin-Linux-Ubuntu-26.04-x86_64.zip",
            };
        }
        #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
        {
            match self {
                Self::Chat => "-bin-win-cpu-x64.zip",
                Self::Media => "-bin-win-cpu-x64.zip",
            }
        }
    }

    fn asset_prefix(self) -> &'static str {
        match self {
            Self::Chat => "llama-",
            Self::Media => "sd-",
        }
    }
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
    state: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseInfo {
    pub engine: String,
    pub tag: String,
    pub asset_name: String,
    pub download_url: String,
    pub release_url: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub prerelease: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct InstalledManifest {
    engine: String,
    tag: String,
    asset_name: String,
    sha256: String,
    release_url: String,
    installed_unix_seconds: u64,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub phase: &'static str,
    pub completed: u64,
    pub total: u64,
    pub attempt: u8,
}

#[cfg(any(not(feature = "desktop"), test))]
pub fn run_cli(args: Vec<OsString>) -> AppResult<()> {
    let mut args: Vec<OsString> = args.into_iter().collect();
    if args.is_empty() {
        return Err(invalid(
            "用法：engine <status|latest|install|update> [chat|media|all]",
        ));
    }
    let command = args
        .remove(0)
        .into_string()
        .map_err(|_| invalid("引擎命令必须是 UTF-8 文本：status、latest、install 或 update"))?;
    let selected = if args.is_empty()
        || args
            .first()
            .is_some_and(|arg| arg.to_string_lossy().starts_with("--"))
    {
        "all".to_string()
    } else {
        args.remove(0)
            .into_string()
            .map_err(|_| invalid("引擎名称必须是 UTF-8 文本：chat 或 media"))?
    };
    let mut args = args.into_iter();
    let kinds: Vec<EngineKind> = if selected == "all" {
        vec![EngineKind::Chat, EngineKind::Media]
    } else {
        vec![EngineKind::parse(&selected)?]
    };

    if matches!(command.as_str(), "install" | "update") && selected == "all" {
        return Err(invalid(
            "install/update 必须单独指定 chat 或 media；两个引擎的下载和更新彼此独立",
        ));
    }
    let mut root_override: Option<PathBuf> = None;
    let mut allow_prerelease = false;
    while let Some(arg) = args.next() {
        if arg == "--dir" {
            let value = args
                .next()
                .ok_or_else(|| invalid("--dir 后需要一个引擎根目录路径"))?;
            root_override = Some(PathBuf::from(value));
        } else if arg == "--allow-prerelease" {
            allow_prerelease = true;
        } else {
            return Err(invalid(format!(
                "无法识别的参数：{}",
                arg.to_string_lossy()
            )));
        }
    }

    match command.as_str() {
        "status" => {
            let root = match root_override {
                Some(root) => root,
                None => default_engines_root()?,
            };
            for kind in kinds {
                print_status(kind, &root)?;
            }
        }
        "latest" => {
            for kind in kinds {
                let release = latest_release(kind, true)?;
                let channel = if release.prerelease {
                    "预览版；安装需显式传入 --allow-prerelease"
                } else {
                    "稳定版"
                };
                println!(
                    "{} 最新可用：{}（{}，{}，SHA-256 {}）\n  {}\n  {}",
                    release.engine,
                    release.tag,
                    channel,
                    format_bytes(release.size_bytes),
                    release.sha256,
                    release.asset_name,
                    release.release_url
                );
            }
        }
        "install" | "update" => {
            ensure_supported_install_platform()?;
            let root = match root_override {
                Some(root) => root,
                None => default_engines_root()?,
            };
            for kind in kinds {
                let release = latest_release(kind, allow_prerelease)?;
                println!(
                    "准备安装 {} {}（{}）",
                    release.engine,
                    release.tag,
                    format_bytes(release.size_bytes)
                );
                if release.prerelease {
                    println!("注意：这是官方预览版 Release。\n  {}", release.release_url);
                }
                let installed = install_release(&release, &root, print_progress)?;
                println!("{}", installed.display());
            }
        }
        "help" | "--help" | "-h" => print_help(),
        _ => return Err(invalid(format!("未知引擎命令：{command}"))),
    }
    Ok(())
}

#[cfg(any(not(feature = "desktop"), test))]
fn print_help() {
    println!(
        "引擎管理：\n  engine status [chat|media|all] [--dir PATH]\n  engine latest [chat|media|all]\n  engine install [chat|media|all] [--dir PATH] [--allow-prerelease]\n  engine update [chat|media|all] [--dir PATH] [--allow-prerelease]\n\nchat 独立安装 llama.cpp；media 安装 stable-diffusion.cpp，供图像与视频共用。\n下载的压缩包先按 GitHub API 提供的 SHA-256 校验，再解压；本命令不会运行已下载的程序。"
    );
}

fn ensure_supported_install_platform() -> AppResult<()> {
    if cfg!(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64")
    )) {
        Ok(())
    } else {
        Err(invalid(
            "当前仅支持 Windows x64 或 Linux x86_64 CPU 官方构建；请在受支持的平台运行安装命令",
        ))
    }
}

fn default_engines_root() -> AppResult<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let local = env::var_os("LOCALAPPDATA")
            .ok_or_else(|| invalid("未找到 LOCALAPPDATA，无法确定当前用户的数据目录"))?;
        return Ok(PathBuf::from(local).join("LocM").join("engines"));
    }
    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME").ok_or_else(|| invalid("未找到 HOME 环境变量"))?;
        return Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("LocM")
            .join("engines"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
            return Ok(PathBuf::from(data_home).join("locm").join("engines"));
        }
        let home = env::var_os("HOME").ok_or_else(|| invalid("未找到 HOME 环境变量"))?;
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("locm")
            .join("engines"));
    }
    #[allow(unreachable_code)]
    Err(invalid("无法确定引擎数据目录；请通过 --dir 指定"))
}

fn client() -> AppResult<Client> {
    let allowed_redirect = |attempt: reqwest::redirect::Attempt<'_>| {
        let url = attempt.url();
        let host = url.host_str().unwrap_or_default();
        if url.scheme() == "https" && is_allowed_github_host(host) {
            attempt.follow()
        } else {
            attempt.error("拒绝跳转到非 GitHub 官方 HTTPS 主机")
        }
    };
    Ok(Client::builder()
        .user_agent(concat!("locm-engine-manager/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        .redirect(Policy::custom(allowed_redirect))
        .build()?)
}

fn is_allowed_github_host(host: &str) -> bool {
    [
        "github.com",
        "api.github.com",
        "release-assets.githubusercontent.com",
        "objects.githubusercontent.com",
        "github-releases.githubusercontent.com",
    ]
    .iter()
    .any(|allowed| host.eq_ignore_ascii_case(allowed))
}

pub fn latest_release(kind: EngineKind, permit_prerelease: bool) -> AppResult<ReleaseInfo> {
    let client = client()?;
    let repo = kind.api_repo();
    let releases = if kind == EngineKind::Media {
        let endpoint = format!("https://api.github.com/repos/{repo}/releases/latest");
        let release: GitHubRelease = get_json(&client, &endpoint)?;
        vec![release]
    } else {
        // llama.cpp 的语义化稳定 Release 可能不附带 Windows 二进制；从官方 Release 列表中
        // 选择最近一个确实提供 CPU x64 ZIP 且带 GitHub SHA-256 digest 的版本。
        let mut all = Vec::new();
        for page in 1..=5 {
            let endpoint =
                format!("https://api.github.com/repos/{repo}/releases?per_page=100&page={page}");
            let mut page_items: Vec<GitHubRelease> = get_json(&client, &endpoint)?;
            let empty = page_items.is_empty();
            all.append(&mut page_items);
            if empty {
                break;
            }
        }
        all
    };

    let mut candidates = Vec::new();
    for release in releases {
        if release.draft || (release.prerelease && !permit_prerelease) {
            continue;
        }
        if let Some(info) = release_to_info(kind, &release)? {
            candidates.push(info);
        }
        if kind == EngineKind::Media && !candidates.is_empty() {
            break;
        }
    }
    candidates.into_iter().next().ok_or_else(|| {
        let extra = if kind == EngineKind::Chat && !permit_prerelease {
            "；若接受官方预览版，可显式使用 --allow-prerelease"
        } else {
            ""
        };
        invalid(format!(
            "在官方 {} Release 中未找到符合当前平台要求、并带 GitHub SHA-256 digest 的 CPU x64 资产{}",
            kind.api_repo(),
            extra
        ))
    })
}

#[cfg(feature = "desktop")]
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledVersion {
    pub tag: String,
    pub sha256: String,
}

#[cfg(feature = "desktop")]
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStatus {
    pub engine: String,
    pub display_name: String,
    pub purpose: String,
    pub install_path: String,
    pub installed_versions: Vec<InstalledVersion>,
}

#[cfg(feature = "desktop")]
pub fn desktop_statuses() -> AppResult<Vec<EngineStatus>> {
    let root = default_engines_root()?;
    Ok(vec![
        engine_status(EngineKind::Chat, &root)?,
        engine_status(EngineKind::Media, &root)?,
    ])
}

#[cfg(feature = "desktop")]
pub fn desktop_latest(engine: &str) -> AppResult<ReleaseInfo> {
    latest_release(EngineKind::parse(engine)?, true)
}

#[cfg(feature = "desktop")]
pub fn desktop_install<F>(
    engine: &str,
    permit_prerelease: bool,
    expected_tag: &str,
    progress: F,
) -> AppResult<EngineStatus>
where
    F: FnMut(ProgressEvent),
{
    ensure_supported_install_platform()?;
    let kind = EngineKind::parse(engine)?;
    validate_expected_release_tag(expected_tag, expected_tag)?;
    let root = default_engines_root()?;
    let release = latest_release(kind, permit_prerelease)?;
    validate_expected_release_tag(expected_tag, &release.tag)?;
    install_release(&release, &root, progress)?;
    engine_status(kind, &root)
}

#[cfg(feature = "desktop")]
pub fn desktop_install_supported() -> bool {
    cfg!(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64")
    ))
}

#[cfg(feature = "desktop")]
fn engine_status(kind: EngineKind, engines_root: &Path) -> AppResult<EngineStatus> {
    let engine_root = engines_root.join(kind.id());
    let mut installed_versions = Vec::new();
    if engine_root.is_dir() {
        for entry in fs::read_dir(&engine_root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.')
            {
                continue;
            }
            let manifest_path = entry.path().join(INSTALL_MANIFEST);
            if let Ok(text) = fs::read_to_string(manifest_path) {
                if let Ok(manifest) = serde_json::from_str::<InstalledManifest>(&text) {
                    if manifest.engine == kind.id() {
                        installed_versions.push(InstalledVersion {
                            tag: manifest.tag,
                            sha256: manifest.sha256,
                        });
                    }
                }
            }
        }
    }
    installed_versions.sort_by(|left, right| left.tag.cmp(&right.tag));
    let (display_name, purpose) = match kind {
        EngineKind::Chat => ("llama.cpp", "对话模型运行时"),
        EngineKind::Media => ("stable-diffusion.cpp", "图像与视频共用运行时"),
    };
    Ok(EngineStatus {
        engine: kind.id().to_string(),
        display_name: display_name.to_string(),
        purpose: purpose.to_string(),
        install_path: engine_root.display().to_string(),
        installed_versions,
    })
}

fn get_json<T: for<'de> Deserialize<'de>>(client: &Client, url: &str) -> AppResult<T> {
    let response = client.get(url).send()?.error_for_status()?;
    Ok(response.json()?)
}

fn release_to_info(kind: EngineKind, release: &GitHubRelease) -> AppResult<Option<ReleaseInfo>> {
    if !safe_component(&release.tag_name) {
        return Err(invalid("官方 Release 返回了不安全的版本标签；拒绝继续"));
    }
    let expected_release_url = format!(
        "https://github.com/{}/releases/tag/{}",
        kind.api_repo(),
        release.tag_name
    );
    if release.html_url != expected_release_url {
        return Err(invalid(format!(
            "Release 页面不属于预期官方仓库：{}",
            release.html_url
        )));
    }

    let matching: Vec<&GitHubAsset> = release
        .assets
        .iter()
        .filter(|asset| {
            asset.name.starts_with(kind.asset_prefix()) && asset.name.ends_with(kind.asset_suffix())
        })
        .collect();
    if matching.is_empty() {
        return Ok(None);
    }
    if matching.len() != 1 {
        return Err(invalid(format!(
            "Release {} 存在多个匹配的 CPU x64 资产；为避免选错，停止安装",
            release.tag_name
        )));
    }
    let asset = matching[0];
    if asset.state != "uploaded" || asset.size == 0 || asset.size > MAX_ARCHIVE_BYTES {
        return Err(invalid(format!(
            "官方资产状态或大小不符合安全限制：{} ({} bytes, {})",
            asset.name, asset.size, asset.state
        )));
    }
    if !safe_component(&asset.name)
        || !(asset.name.ends_with(".zip") || asset.name.ends_with(".tar.gz"))
    {
        return Err(invalid("官方资产文件名不是安全的 ZIP 名称；拒绝继续"));
    }
    let sha256 = parse_sha256(asset.digest.as_deref())?;
    let expected_asset_url = format!(
        "https://github.com/{}/releases/download/{}/{}",
        kind.api_repo(),
        release.tag_name,
        asset.name
    );
    if asset.browser_download_url != expected_asset_url {
        return Err(invalid(format!(
            "官方资产下载地址与仓库/版本/文件名不匹配：{}",
            asset.browser_download_url
        )));
    }
    Ok(Some(ReleaseInfo {
        engine: kind.id().to_string(),
        tag: release.tag_name.clone(),
        asset_name: asset.name.clone(),
        download_url: asset.browser_download_url.clone(),
        release_url: release.html_url.clone(),
        size_bytes: asset.size,
        sha256,
        prerelease: release.prerelease,
    }))
}

fn parse_sha256(digest: Option<&str>) -> AppResult<String> {
    let value = digest.ok_or_else(|| {
        invalid("官方 Release API 没有提供该资产的 SHA-256 digest；拒绝无校验下载")
    })?;
    let hash = value
        .strip_prefix("sha256:")
        .ok_or_else(|| invalid("官方 Release digest 不是 SHA-256 格式；拒绝继续"))?;
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("官方 SHA-256 digest 格式无效；拒绝继续"));
    }
    Ok(hash.to_ascii_lowercase())
}

fn install_release<F>(
    release: &ReleaseInfo,
    engines_root: &Path,
    mut progress: F,
) -> AppResult<PathBuf>
where
    F: FnMut(ProgressEvent),
{
    let kind = if release.engine == EngineKind::Chat.id() {
        EngineKind::Chat
    } else if release.engine == EngineKind::Media.id() {
        EngineKind::Media
    } else {
        return Err(invalid("ReleaseInfo 引擎标识不认识；拒绝写入安装目录"));
    };
    if !safe_component(&release.tag) || !safe_component(&release.asset_name) {
        return Err(invalid("版本标签或资产名称不安全；拒绝继续"));
    }
    let expected_sha = parse_sha256(Some(&format!("sha256:{}", release.sha256)))?;
    let engine_root = engines_root.join(kind.id());
    let target = engine_root.join(&release.tag);
    if target.exists() {
        if installed_matches(&target, release)? {
            return Ok(target);
        }
        return Err(invalid(format!(
            "目标版本目录已存在但不能由本程序验证，未覆盖：{}",
            target.display()
        )));
    }

    fs::create_dir_all(&engine_root)?;
    let run_id = format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        STAGING_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let ext = if release.asset_name.ends_with(".tar.gz") {
        "tar.gz"
    } else {
        "zip"
    };
    let archive_path = engine_root.join(format!(".download-{}-{run_id}.{ext}.part", release.tag));
    match fs::symlink_metadata(&archive_path) {
        Ok(_) => return Err(invalid("临时下载路径冲突；为避免覆盖已有文件，已停止")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut successful_attempt = 0;
    for attempt in 1..=DOWNLOAD_ATTEMPTS {
        progress(ProgressEvent {
            phase: "download",
            completed: 0,
            total: release.size_bytes,
            attempt,
        });
        let mut created_by_this_attempt = false;
        match download_once(
            release,
            &archive_path,
            attempt,
            &mut created_by_this_attempt,
            &mut progress,
        )
        .and_then(|()| verify_sha256(&archive_path, &expected_sha))
        {
            Ok(()) => {
                successful_attempt = attempt;
                break;
            }
            Err(error) => {
                if created_by_this_attempt {
                    cleanup_download_file(&archive_path);
                }
                if attempt == DOWNLOAD_ATTEMPTS {
                    return Err(error);
                }
                eprintln!(
                    "下载或校验失败（第 {attempt}/{DOWNLOAD_ATTEMPTS} 次）：{error}；准备重试"
                );
                thread::sleep(Duration::from_millis(u64::from(attempt) * 500));
            }
        }
    }

    progress(ProgressEvent {
        phase: "verify",
        completed: release.size_bytes,
        total: release.size_bytes,
        attempt: successful_attempt,
    });
    let stage = engine_root.join(format!(".staging-{}-{run_id}", release.tag));
    if let Err(error) = fs::create_dir(&stage) {
        cleanup_download_file(&archive_path);
        return Err(error.into());
    }

    let prepare_result: AppResult<()> = (|| {
        if release.asset_name.ends_with(".tar.gz") {
            extract_tar_gz_safely(&archive_path, &stage, &mut progress)?;
        } else {
            extract_zip_safely(&archive_path, &stage, &mut progress)?;
        }
        let manifest = InstalledManifest {
            engine: release.engine.clone(),
            tag: release.tag.clone(),
            asset_name: release.asset_name.clone(),
            sha256: expected_sha,
            release_url: release.release_url.clone(),
            installed_unix_seconds: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        let manifest_path = stage.join(INSTALL_MANIFEST);
        let mut manifest_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(manifest_path)?;
        serde_json::to_writer_pretty(&mut manifest_file, &manifest)?;
        manifest_file.write_all(b"\n")?;
        manifest_file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = prepare_result {
        cleanup_install_paths(&stage, &archive_path);
        return Err(error);
    }

    if target.exists() {
        cleanup_install_paths(&stage, &archive_path);
        return Err(invalid(format!(
            "安装期间目标目录被创建，未覆盖：{}",
            target.display()
        )));
    }
    if let Err(error) = fs::rename(&stage, &target) {
        cleanup_install_paths(&stage, &archive_path);
        return Err(error.into());
    }
    cleanup_download_file(&archive_path);
    progress(ProgressEvent {
        phase: "complete",
        completed: 1,
        total: 1,
        attempt: successful_attempt,
    });
    Ok(target)
}

fn cleanup_download_file(path: &Path) {
    if let Err(error) = fs::remove_file(path) {
        if error.kind() != io::ErrorKind::NotFound {
            eprintln!("警告：无法清理临时下载文件 {}：{error}", path.display());
        }
    }
}

fn cleanup_install_paths(stage: &Path, archive: &Path) {
    if let Err(error) = fs::remove_dir_all(stage) {
        if error.kind() != io::ErrorKind::NotFound {
            eprintln!("警告：无法清理暂存目录 {}：{error}", stage.display());
        }
    }
    cleanup_download_file(archive);
}

fn installed_matches(target: &Path, release: &ReleaseInfo) -> AppResult<bool> {
    let manifest_path = target.join(INSTALL_MANIFEST);
    let content = match fs::read_to_string(manifest_path) {
        Ok(content) => content,
        Err(_) => return Ok(false),
    };
    let manifest: InstalledManifest = match serde_json::from_str(&content) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(false),
    };
    Ok(manifest.engine == release.engine
        && manifest.tag == release.tag
        && manifest.asset_name == release.asset_name
        && manifest.sha256.eq_ignore_ascii_case(&release.sha256))
}

fn download_once<F>(
    release: &ReleaseInfo,
    archive_path: &Path,
    attempt: u8,
    created_by_this_attempt: &mut bool,
    progress: &mut F,
) -> AppResult<()>
where
    F: FnMut(ProgressEvent),
{
    let response = client()?
        .get(&release.download_url)
        .send()?
        .error_for_status()?;
    validate_final_download_url(&response)?;
    if let Some(length) = response.content_length() {
        if length != release.size_bytes {
            return Err(invalid(format!(
                "下载服务器返回的 Content-Length 与官方资产大小不一致（{length} vs {}）",
                release.size_bytes
            )));
        }
    }
    write_response(
        response,
        release.size_bytes,
        archive_path,
        attempt,
        created_by_this_attempt,
        progress,
    )
}

fn validate_final_download_url(response: &Response) -> AppResult<()> {
    let url = response.url();
    let host = url.host_str().unwrap_or_default();
    if url.scheme() != "https" || !is_allowed_github_host(host) {
        return Err(invalid(format!(
            "下载最终地址不属于 GitHub 官方 HTTPS 域名：{}",
            url
        )));
    }
    Ok(())
}

fn write_response<F>(
    mut response: Response,
    expected_size: u64,
    path: &Path,
    attempt: u8,
    created_by_this_attempt: &mut bool,
    progress: &mut F,
) -> AppResult<()>
where
    F: FnMut(ProgressEvent),
{
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    *created_by_this_attempt = true;
    let mut buffer = [0u8; 64 * 1024];
    let mut received = 0u64;
    loop {
        let count = response.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        received = received
            .checked_add(count as u64)
            .ok_or_else(|| invalid("下载大小溢出"))?;
        if received > expected_size || received > MAX_ARCHIVE_BYTES {
            return Err(invalid("下载内容超过官方声明大小；已停止并将删除临时文件"));
        }
        file.write_all(&buffer[..count])?;
        progress(ProgressEvent {
            phase: "download",
            completed: received,
            total: expected_size,
            attempt,
        });
    }
    file.flush()?;
    file.sync_all()?;
    if received != expected_size {
        return Err(invalid(format!(
            "下载不完整：收到 {received} bytes，官方声明 {} bytes",
            expected_size
        )));
    }
    Ok(())
}

fn verify_sha256(path: &Path, expected: &str) -> AppResult<()> {
    let actual = sha256_file(path)?;
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(invalid(format!(
            "SHA-256 校验失败：期望 {expected}，实际 {actual}"
        )));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> AppResult<String> {
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

fn extract_tar_gz_safely<F>(
    archive_path: &Path,
    destination: &Path,
    progress: &mut F,
) -> AppResult<()>
where
    F: FnMut(ProgressEvent),
{
    let root = destination.canonicalize()?;
    let mut archive = Archive::new(GzDecoder::new(File::open(archive_path)?));
    let mut total = 0u64;
    let mut count = 0usize;
    for item in archive.entries()? {
        count += 1;
        if count > MAX_ARCHIVE_ENTRIES {
            return Err(invalid("tar.gz 文件条目过多；拒绝解包"));
        }
        let mut entry = item?;
        let ty = entry.header().entry_type();
        if !ty.is_file() && !ty.is_dir() {
            return Err(invalid("tar.gz 包含链接或特殊文件；拒绝解包"));
        }
        let name = entry.path()?.into_owned();
        if !name.is_relative()
            || name.to_string_lossy().contains('\\')
            || !name.components().all(|c| matches!(c, Component::Normal(_)))
        {
            return Err(invalid(format!("tar.gz 包含不安全路径：{name:?}")));
        }
        let out = root.join(&name);
        let size = entry.size();
        total = total
            .checked_add(size)
            .ok_or_else(|| invalid("tar.gz 解压总大小溢出"))?;
        if size > MAX_SINGLE_FILE_BYTES || total > MAX_UNPACKED_BYTES {
            return Err(invalid("tar.gz 解压内容超过大小安全限制；拒绝解包"));
        }
        ensure_safe_extraction_path(&root, &out)?;
        if ty.is_dir() {
            fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
                ensure_safe_extraction_path(&root, parent)?;
            }
            let mut file = OpenOptions::new().write(true).create_new(true).open(&out)?;
            let copied = io::copy(&mut entry.by_ref().take(size + 1), &mut file)?;
            if copied != size {
                return Err(invalid("tar.gz 文件解压大小不一致"));
            }
            file.flush()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = if entry.header().mode()? & 0o111 != 0 {
                    0o755
                } else {
                    0o644
                };
                fs::set_permissions(&out, fs::Permissions::from_mode(mode))?;
            }
            ensure_safe_extraction_path(&root, &out)?;
        }
        progress(ProgressEvent {
            phase: "extract",
            completed: count as u64,
            total: 0,
            attempt: DOWNLOAD_ATTEMPTS,
        });
    }
    Ok(())
}

fn extract_zip_safely<F>(archive_path: &Path, destination: &Path, progress: &mut F) -> AppResult<()>
where
    F: FnMut(ProgressEvent),
{
    let extraction_root = destination.canonicalize()?;
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(invalid("ZIP 文件条目过多；拒绝解包"));
    }
    let file_count = archive.len() as u64;
    let mut total_unpacked = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.name().to_string();
        if !safe_zip_entry_name(&name) {
            return Err(invalid(format!("ZIP 包含不安全路径：{name:?}")));
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(invalid(format!("ZIP 包含符号链接：{name:?}")));
        }
        let relative = entry
            .enclosed_name()
            .ok_or_else(|| invalid(format!("ZIP 路径越界：{name:?}")))?;
        if relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(invalid(format!("ZIP 路径含非普通路径组件：{name:?}")));
        }
        total_unpacked = total_unpacked
            .checked_add(entry.size())
            .ok_or_else(|| invalid("ZIP 解压总大小溢出"))?;
        if entry.size() > MAX_SINGLE_FILE_BYTES || total_unpacked > MAX_UNPACKED_BYTES {
            return Err(invalid("ZIP 解压内容超过大小安全限制；拒绝解包"));
        }
        let output_path = extraction_root.join(&relative);
        if entry.is_dir() {
            ensure_safe_extraction_path(&extraction_root, &output_path)?;
            fs::create_dir_all(&output_path)?;
            ensure_safe_extraction_path(&extraction_root, &output_path)?;
        } else {
            if let Some(parent) = output_path.parent() {
                fs::create_dir_all(parent)?;
                ensure_safe_extraction_path(&extraction_root, parent)?;
            }
            ensure_safe_extraction_path(&extraction_root, &output_path)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output_path)?;
            let expected = entry.size();
            let copied = io::copy(&mut entry.by_ref().take(expected + 1), &mut output)?;
            if copied != expected {
                return Err(invalid(format!("ZIP 文件解压大小不一致：{name:?}")));
            }
            output.flush()?;
            ensure_safe_extraction_path(&extraction_root, &output_path)?;
        }
        progress(ProgressEvent {
            phase: "extract",
            completed: index as u64 + 1,
            total: file_count,
            attempt: DOWNLOAD_ATTEMPTS,
        });
    }
    Ok(())
}

fn safe_zip_entry_name(name: &str) -> bool {
    let without_trailing_slash = name.strip_suffix('/').unwrap_or(name);
    !without_trailing_slash.is_empty()
        && !without_trailing_slash.contains('\\')
        && !without_trailing_slash.contains('\0')
        && !without_trailing_slash.contains(':')
        && !without_trailing_slash.starts_with('/')
        && without_trailing_slash.split('/').all(safe_windows_segment)
}

fn safe_windows_segment(segment: &str) -> bool {
    if segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment.ends_with('.')
        || segment.ends_with(' ')
        || segment.chars().any(|character| {
            character.is_control() || matches!(character, '<' | '>' | '"' | '|' | '?' | '*')
        })
    {
        return false;
    }
    let base = segment
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    let reserved_name = matches!(
        base.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "CONIN$"
            | "CONOUT$"
            | "CLOCK$"
            | "COM¹"
            | "COM²"
            | "COM³"
            | "LPT¹"
            | "LPT²"
            | "LPT³"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    );
    !reserved_name
}

fn ensure_safe_extraction_path(root: &Path, path: &Path) -> AppResult<()> {
    if !path.starts_with(root) {
        return Err(invalid(format!(
            "ZIP 解压目标超出暂存目录：{}",
            path.display()
        )));
    }
    let mut current = path.to_path_buf();
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(invalid(format!(
                        "ZIP 解压路径经过符号链接或 reparse point：{}",
                        current.display()
                    )));
                }
                if !current.canonicalize()?.starts_with(root) {
                    return Err(invalid(format!(
                        "ZIP 解压路径解析后超出暂存目录：{}",
                        current.display()
                    )));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if current == root {
            break;
        }
        if !current.pop() || !current.starts_with(root) {
            return Err(invalid("无法确认 ZIP 解压路径位于暂存目录内"));
        }
    }
    Ok(())
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && safe_windows_segment(value)
}

fn validate_expected_release_tag(expected_tag: &str, actual_tag: &str) -> AppResult<()> {
    if !safe_component(expected_tag) {
        return Err(invalid("确认的版本标签无效或为空；请重新检查最新版本"));
    }
    if expected_tag != actual_tag {
        return Err(invalid(format!(
            "确认的版本已变更，请重新检查后再安装；确认 {expected_tag}，当前 {actual_tag}"
        )));
    }
    Ok(())
}

#[cfg(any(not(feature = "desktop"), test))]
fn print_status(kind: EngineKind, engines_root: &Path) -> AppResult<()> {
    let engine_root = engines_root.join(kind.id());
    println!("{}：", kind.id());
    if !engine_root.is_dir() {
        println!("  未安装；安装目录：{}", engine_root.display());
        return Ok(());
    }
    let mut versions = Vec::new();
    for entry in fs::read_dir(&engine_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let manifest_path = entry.path().join(INSTALL_MANIFEST);
        if let Ok(text) = fs::read_to_string(manifest_path) {
            if let Ok(manifest) = serde_json::from_str::<InstalledManifest>(&text) {
                if manifest.engine == kind.id() {
                    versions.push((manifest.tag, manifest.sha256));
                }
            }
        }
    }
    versions.sort_by(|left, right| left.0.cmp(&right.0));
    if versions.is_empty() {
        println!(
            "  未发现可验证的安装版本；安装目录：{}",
            engine_root.display()
        );
    } else {
        for (tag, sha256) in versions {
            println!("  已安装 {tag}（SHA-256 {sha256}）");
        }
        println!("  路径：{}", engine_root.display());
    }
    Ok(())
}

#[cfg(any(not(feature = "desktop"), test))]
fn print_progress(event: ProgressEvent) {
    match event.phase {
        "download" => {
            let percent = if event.total == 0 {
                0.0
            } else {
                100.0 * event.completed as f64 / event.total as f64
            };
            eprint!(
                "\r下载中：{percent:5.1}%（{} / {}，第 {}/{} 次）",
                format_bytes(event.completed),
                format_bytes(event.total),
                event.attempt,
                DOWNLOAD_ATTEMPTS
            );
            let _ = io::stderr().flush();
        }
        "verify" => eprintln!("\n下载完成，正在校验 SHA-256…"),
        "extract" => {
            eprint!("\r安全解包：{} / {} 个条目", event.completed, event.total);
            let _ = io::stderr().flush();
        }
        "complete" => eprintln!("\n安装完成（尚未启动引擎程序）。"),
        _ => {}
    }
}

#[cfg(any(not(feature = "desktop"), test))]
fn format_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes < 1024 * 1024 {
        format!("{:.0} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB)
    }
}

fn invalid(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::{write::SimpleFileOptions, ZipWriter};

    fn release(kind: EngineKind) -> GitHubRelease {
        let repo = kind.api_repo();
        let tag = match kind {
            EngineKind::Chat => "b12498",
            EngineKind::Media => "master-1106-532d2c1",
        };
        let asset = match kind {
            EngineKind::Chat if cfg!(all(target_os = "linux", target_arch = "x86_64")) => {
                "llama-b12498-bin-ubuntu-x64.tar.gz"
            }
            EngineKind::Media if cfg!(all(target_os = "linux", target_arch = "x86_64")) => {
                "sd-master-532d2c1-bin-Linux-Ubuntu-26.04-x86_64.zip"
            }
            EngineKind::Chat => "llama-b12498-bin-win-cpu-x64.zip",
            EngineKind::Media => "sd-master-532d2c1-bin-win-cpu-x64.zip",
        };
        GitHubRelease {
            tag_name: tag.into(),
            html_url: format!("https://github.com/{repo}/releases/tag/{tag}"),
            draft: false,
            prerelease: false,
            assets: vec![GitHubAsset {
                name: asset.into(),
                browser_download_url: format!(
                    "https://github.com/{repo}/releases/download/{tag}/{asset}"
                ),
                size: 10,
                digest: Some(format!("sha256:{}", "a".repeat(64))),
                state: "uploaded".into(),
            }],
        }
    }

    #[test]
    fn selects_only_exact_cpu_x64_asset_from_expected_repo() {
        let info = release_to_info(EngineKind::Media, &release(EngineKind::Media))
            .unwrap()
            .unwrap();
        assert_eq!(info.asset_name, release(EngineKind::Media).assets[0].name);
        assert_eq!(info.sha256, "a".repeat(64));

        let chat = release_to_info(EngineKind::Chat, &release(EngineKind::Chat))
            .unwrap()
            .unwrap();
        assert_eq!(chat.asset_name, release(EngineKind::Chat).assets[0].name);

        let mut invalid_release = release(EngineKind::Media);
        invalid_release.assets[0].browser_download_url = "https://example.com/evil.zip".into();
        assert!(release_to_info(EngineKind::Media, &invalid_release).is_err());
    }

    #[test]
    fn rejects_missing_or_malformed_digest() {
        assert!(parse_sha256(None).is_err());
        assert!(parse_sha256(Some("md5:abcd")).is_err());
        assert!(parse_sha256(Some("sha256:abcd")).is_err());
    }

    #[test]
    fn rejects_path_traversal_and_windows_path_separators() {
        assert!(!safe_zip_entry_name("../outside.exe"));
        assert!(!safe_zip_entry_name("folder\\..\\outside.exe"));
        assert!(!safe_zip_entry_name("C:/outside.exe"));
        assert!(safe_zip_entry_name("bin/sd-cli.exe"));
        assert!(safe_zip_entry_name("bin/"));
        assert!(!safe_zip_entry_name("bin//"));
        assert!(!safe_zip_entry_name("CON"));
        assert!(!safe_zip_entry_name("con.txt"));
        assert!(!safe_zip_entry_name("COM1.log"));
        assert!(!safe_zip_entry_name("file."));
        assert!(!safe_zip_entry_name("file "));
        assert!(!safe_component("CON"));
        assert!(!safe_component("LPT9"));
        assert!(!safe_component("master-1106-532d2c1."));
        assert!(safe_component("master-1106-532d2c1"));
    }

    #[test]
    fn redirect_allowlist_is_exact_and_case_insensitive() {
        assert!(is_allowed_github_host(
            "RELEASE-ASSETS.githubusercontent.com"
        ));
        assert!(!is_allowed_github_host("malicious.githubusercontent.com"));
        assert!(!is_allowed_github_host("github.com.attacker.example"));
    }

    #[test]
    fn rejects_extraction_paths_outside_staging_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("stage");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        assert!(ensure_safe_extraction_path(&root.canonicalize().unwrap(), &outside).is_err());
    }

    #[test]
    fn install_requires_selecting_one_engine() {
        let error = run_cli(vec![OsString::from("install")]).unwrap_err();
        assert!(error.to_string().contains("单独指定 chat 或 media"));
    }

    #[test]
    fn status_accepts_dir_without_explicit_engine() {
        let temp = tempfile::tempdir().unwrap();
        run_cli(vec![
            OsString::from("status"),
            OsString::from("--dir"),
            temp.path().as_os_str().to_os_string(),
        ])
        .unwrap();
    }

    #[test]
    fn refuses_to_extract_zip_path_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("bad.zip");
        let file = File::create(&zip_path).unwrap();
        let mut writer = ZipWriter::new(file);
        writer
            .start_file("../outside.txt", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"no").unwrap();
        writer.finish().unwrap();
        let destination = temp.path().join("out");
        fs::create_dir(&destination).unwrap();
        let result = extract_zip_safely(&zip_path, &destination, &mut |_| {});
        assert!(result.is_err());
        assert!(!temp.path().join("outside.txt").exists());
    }

    #[test]
    fn extracts_regular_files_beneath_a_relative_staging_path() {
        let temp = tempfile::tempdir_in(".").unwrap();
        let zip_path = temp.path().join("good.zip");
        let file = File::create(&zip_path).unwrap();
        let mut writer = ZipWriter::new(file);
        writer
            .start_file("bin/engine.exe", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"test-data").unwrap();
        writer.finish().unwrap();
        let destination = temp.path().join("out");
        fs::create_dir(&destination).unwrap();
        extract_zip_safely(&zip_path, &destination, &mut |_| {}).unwrap();
        assert_eq!(
            fs::read(destination.join("bin/engine.exe")).unwrap(),
            b"test-data"
        );
    }

    #[test]
    fn refuses_to_extract_zip_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let zip_path = temp.path().join("symlink.zip");
        let file = File::create(&zip_path).unwrap();
        let mut writer = ZipWriter::new(file);
        writer
            .start_file(
                "link.txt",
                SimpleFileOptions::default().unix_permissions(0o120777),
            )
            .unwrap();
        writer.write_all(b"target.txt").unwrap();
        writer.finish().unwrap();
        let mut bytes = fs::read(&zip_path).unwrap();
        let central_header = bytes
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .unwrap();
        bytes[central_header + 5] = 3; // ZIP creator system: Unix.
        bytes[central_header + 38..central_header + 42]
            .copy_from_slice(&(0o120777u32 << 16).to_le_bytes());
        fs::write(&zip_path, bytes).unwrap();
        let destination = temp.path().join("out");
        fs::create_dir(&destination).unwrap();
        assert!(extract_zip_safely(&zip_path, &destination, &mut |_| {}).is_err());
        assert!(!destination.join("link.txt").exists());
    }

    #[test]
    fn extracts_tar_gz_regular_file_and_restores_executable_mode() {
        use flate2::{write::GzEncoder, Compression};
        use tar::Builder;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("good.tar.gz");
        let encoder = GzEncoder::new(File::create(&path).unwrap(), Compression::default());
        let mut b = Builder::new(encoder);
        let mut h = tar::Header::new_gnu();
        h.set_path("bin/engine").unwrap();
        h.set_mode(0o755);
        h.set_size(4);
        h.set_cksum();
        b.append(&h, &b"test"[..]).unwrap();
        b.finish().unwrap();
        b.into_inner().unwrap().finish().unwrap();
        let out = temp.path().join("out");
        fs::create_dir(&out).unwrap();
        extract_tar_gz_safely(&path, &out, &mut |_| {}).unwrap();
        assert_eq!(fs::read(out.join("bin/engine")).unwrap(), b"test");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(out.join("bin/engine"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn rejects_tar_gz_traversal_and_symlink() {
        use flate2::{write::GzEncoder, Compression};
        use tar::Builder;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bad.tar.gz");
        let encoder = GzEncoder::new(File::create(&path).unwrap(), Compression::default());
        let mut b = Builder::new(encoder);
        let mut h = tar::Header::new_gnu();
        h.set_path("link").unwrap();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_link_name("../outside").unwrap();
        h.set_size(0);
        h.set_cksum();
        b.append(&h, &b""[..]).unwrap();
        b.finish().unwrap();
        b.into_inner().unwrap().finish().unwrap();
        let out = temp.path().join("out");
        fs::create_dir(&out).unwrap();
        assert!(extract_tar_gz_safely(&path, &out, &mut |_| {}).is_err());
        assert!(!out.join("link").exists());
    }

    #[test]
    fn engine_directories_are_independent() {
        let temp = tempfile::tempdir().unwrap();
        let chat = temp.path().join(EngineKind::Chat.id());
        let media = temp.path().join(EngineKind::Media.id());
        fs::create_dir_all(&chat).unwrap();
        fs::create_dir_all(&media).unwrap();
        assert_ne!(chat, media);
    }

    #[test]
    fn expected_release_tag_matching_is_allowed() {
        assert!(validate_expected_release_tag("v1.2.3", "v1.2.3").is_ok());
    }

    #[test]
    fn expected_release_tag_mismatch_is_rejected() {
        let error = validate_expected_release_tag("v1.2.3", "v1.2.4").unwrap_err();
        assert!(error.to_string().contains("确认的版本已变更"));
        assert!(error.to_string().contains("确认 v1.2.3，当前 v1.2.4"));
    }

    #[test]
    fn empty_or_invalid_expected_release_tag_is_rejected() {
        assert!(validate_expected_release_tag("", "v1.2.3").is_err());
        assert!(validate_expected_release_tag("../v1.2.3", "../v1.2.3").is_err());
    }
}
