use serde::Serialize;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;

const MAX_TEXT_BYTES: u64 = 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_TOOL_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_SEARCH_FILES: usize = 500;
const MAX_SEARCH_ENTRIES: usize = 1_000;
const MAX_SEARCH_MATCHES: usize = 64;
const MAX_SEARCH_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SEARCH_PREVIEW_BYTES: usize = 240;
const MAX_LIST_ENTRIES: usize = 500;
const MAX_LIST_DEPTH: usize = 6;
const MAX_COMMAND_RUNTIME: Duration = Duration::from_secs(120);
const COMMAND_OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFile {
    pub path: String,
    pub kind: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    pub path: String,
    pub line_number: usize,
    pub line: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchOutcome {
    pub matches: Vec<SearchMatch>,
    pub files_searched: usize,
    pub files_skipped: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteOutcome {
    pub written: bool,
    pub requires_confirmation: bool,
    pub cancelled: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditFileOutcome {
    pub edited: bool,
    pub requires_confirmation: bool,
    pub cancelled: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateFolderOutcome {
    pub created: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandOutput {
    pub success: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

fn workspace_root(workspace: &str) -> Result<PathBuf, String> {
    let root = PathBuf::from(workspace)
        .canonicalize()
        .map_err(|error| format!("无法打开工作文件夹：{error}"))?;
    if !root.is_dir() {
        return Err("工作文件夹必须是一个目录".to_string());
    }
    if root.components().any(|component| {
        matches!(component, Component::Normal(name) if name.to_string_lossy().eq_ignore_ascii_case(".git"))
    }) {
        return Err("不能将 .git 元数据目录或其子目录设为 Agent 工作区".to_string());
    }
    Ok(root)
}

fn safe_components(relative: &str, allow_root: bool) -> Result<Vec<String>, String> {
    if relative.len() > 1024 || relative.contains('\\') {
        return Err("相对路径无效或过长".to_string());
    }
    let path = Path::new(relative);
    let mut result = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_string_lossy().into_owned();
                if value.is_empty() || value == "." || value == ".." {
                    return Err("相对路径包含不允许的目录项".to_string());
                }
                if value.contains(':')
                    || value.ends_with('.')
                    || value.ends_with(' ')
                    || value.chars().any(char::is_control)
                    || is_windows_device_name(&value)
                {
                    return Err("路径包含 Windows 设备名或不安全字符".to_string());
                }
                if value.eq_ignore_ascii_case(".git") {
                    return Err("Agent 不允许读取或修改 .git 元数据".to_string());
                }
                result.push(value);
            }
            Component::CurDir if allow_root && result.is_empty() => {}
            _ => return Err("只允许使用工作文件夹内的相对路径".to_string()),
        }
    }
    if result.is_empty() && !allow_root {
        return Err("文件路径不能为空".to_string());
    }
    Ok(result)
}

fn is_windows_device_name(component: &str) -> bool {
    let base = component
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            base.strip_prefix(prefix).is_some_and(|suffix| {
                suffix.len() == 1
                    && suffix
                        .chars()
                        .next()
                        .is_some_and(|digit| ('1'..='9').contains(&digit))
            })
        })
}

fn checked_existing(root: &Path, components: &[String]) -> Result<PathBuf, String> {
    let mut current = root.to_path_buf();
    for component in components {
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| format!("无法访问工作区路径 {}：{error}", current.display()))?;
        if metadata.file_type().is_symlink() {
            return Err("Agent 不跟随工作文件夹中的符号链接".to_string());
        }
    }
    let canonical = current
        .canonicalize()
        .map_err(|error| format!("无法解析工作区路径：{error}"))?;
    if !canonical.starts_with(root) {
        return Err("路径超出所选工作文件夹".to_string());
    }
    Ok(canonical)
}

pub fn canonicalize_workspace(workspace: &str) -> Result<String, String> {
    Ok(workspace_root(workspace)?.display().to_string())
}

pub fn list_files(workspace: &str, relative_path: &str) -> Result<Vec<WorkspaceFile>, String> {
    let root = workspace_root(workspace)?;
    let components = safe_components(relative_path, true)?;
    let directory = if components.is_empty() {
        root.clone()
    } else {
        checked_existing(&root, &components)?
    };
    if !directory.is_dir() {
        return Err("列目录工具需要一个文件夹路径".to_string());
    }
    fn walk(
        root: &Path,
        directory: &Path,
        depth: usize,
        out: &mut Vec<WorkspaceFile>,
    ) -> Result<(), String> {
        if depth > MAX_LIST_DEPTH || out.len() >= MAX_LIST_ENTRIES {
            return Ok(());
        }
        let mut entries: Vec<_> = fs::read_dir(directory)
            .map_err(|error| format!("读取目录失败：{error}"))?
            .filter_map(Result::ok)
            .collect();
        entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
        for entry in entries {
            if out.len() >= MAX_LIST_ENTRIES {
                break;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.eq_ignore_ascii_case(".git") {
                continue;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| "目录项超出工作文件夹".to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            if metadata.is_dir() {
                out.push(WorkspaceFile {
                    path: relative,
                    kind: "directory".to_string(),
                    size_bytes: 0,
                });
                let skip_heavy_directory = matches!(
                    name.to_ascii_lowercase().as_str(),
                    "target" | "node_modules" | ".venv" | "venv" | "dist" | "build"
                );
                if depth < MAX_LIST_DEPTH && !skip_heavy_directory {
                    walk(root, &path, depth + 1, out)?;
                }
            } else if metadata.is_file() {
                out.push(WorkspaceFile {
                    path: relative,
                    kind: "file".to_string(),
                    size_bytes: metadata.len(),
                });
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    walk(&root, &directory, 0, &mut entries)?;
    Ok(entries)
}

pub fn search_files(
    workspace: &str,
    relative_path: &str,
    query: &str,
    case_sensitive: bool,
) -> Result<SearchOutcome, String> {
    if query.trim().is_empty() || query.len() > 256 || query.contains('\0') {
        return Err("搜索内容不能为空，且最多为 256 字节".to_string());
    }
    let root = workspace_root(workspace)?;
    let components = safe_components(relative_path, true)?;
    let directory = if components.is_empty() {
        root.clone()
    } else {
        checked_existing(&root, &components)?
    };
    if !directory.is_dir() {
        return Err("搜索起点必须是工作区内的文件夹".to_string());
    }

    struct SearchState<'a> {
        query: &'a str,
        folded_query: String,
        case_sensitive: bool,
        matches: Vec<SearchMatch>,
        files_seen: usize,
        files_searched: usize,
        files_skipped: usize,
        entries_seen: usize,
        bytes_searched: u64,
        truncated: bool,
    }

    fn visit(
        root: &Path,
        directory: &Path,
        depth: usize,
        state: &mut SearchState<'_>,
    ) -> Result<(), String> {
        if depth > MAX_LIST_DEPTH
            || state.files_seen >= MAX_SEARCH_FILES
            || state.entries_seen >= MAX_SEARCH_ENTRIES
            || state.matches.len() >= MAX_SEARCH_MATCHES
        {
            state.truncated = true;
            return Ok(());
        }
        let entry_budget = MAX_SEARCH_ENTRIES.saturating_sub(state.entries_seen);
        let mut entries = Vec::new();
        for entry in
            fs::read_dir(directory).map_err(|error| format!("读取搜索目录失败：{error}"))?
        {
            if entries.len() >= entry_budget {
                state.truncated = true;
                break;
            }
            entries.push(entry.map_err(|error| format!("读取搜索目录项失败：{error}"))?);
        }
        entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
        for entry in entries {
            if state.files_seen >= MAX_SEARCH_FILES
                || state.entries_seen >= MAX_SEARCH_ENTRIES
                || state.matches.len() >= MAX_SEARCH_MATCHES
                || state.bytes_searched >= MAX_SEARCH_TOTAL_BYTES
            {
                state.truncated = true;
                break;
            }
            state.entries_seen += 1;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.eq_ignore_ascii_case(".git") {
                continue;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("检查搜索路径失败：{error}"))?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                let skip_heavy_directory = matches!(
                    name.to_ascii_lowercase().as_str(),
                    "target" | "node_modules" | ".venv" | "venv" | "dist" | "build"
                );
                if skip_heavy_directory || depth >= MAX_LIST_DEPTH {
                    state.truncated = true;
                } else {
                    visit(root, &path, depth + 1, state)?;
                }
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            state.files_seen += 1;
            if metadata.len() > MAX_TEXT_BYTES {
                state.files_skipped += 1;
                continue;
            }
            if metadata.len() > MAX_SEARCH_TOTAL_BYTES - state.bytes_searched {
                state.truncated = true;
                return Ok(());
            }
            let remaining_bytes = MAX_SEARCH_TOTAL_BYTES - state.bytes_searched;
            let read_limit = MAX_TEXT_BYTES.min(remaining_bytes).saturating_add(1);
            let mut data = Vec::with_capacity(metadata.len() as usize);
            fs::File::open(&path)
                .and_then(|file| file.take(read_limit).read_to_end(&mut data))
                .map_err(|error| format!("读取搜索文件 {} 失败：{error}", path.display()))?;
            if data.len() as u64 > remaining_bytes {
                state.truncated = true;
                return Ok(());
            }
            state.bytes_searched = state.bytes_searched.saturating_add(data.len() as u64);
            if data.len() as u64 > MAX_TEXT_BYTES {
                state.files_skipped += 1;
                continue;
            }
            if data.contains(&0) {
                state.files_skipped += 1;
                continue;
            }
            let Ok(text) = std::str::from_utf8(&data) else {
                state.files_skipped += 1;
                continue;
            };
            state.files_searched += 1;
            let relative = path
                .strip_prefix(root)
                .map_err(|_| "搜索路径超出所选工作文件夹".to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            for (index, line) in text.lines().enumerate() {
                let found = if state.case_sensitive {
                    line.contains(state.query)
                } else {
                    line.to_lowercase().contains(&state.folded_query)
                };
                if !found {
                    continue;
                }
                let mut end = line.len().min(MAX_SEARCH_PREVIEW_BYTES);
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                let mut preview = line[..end].to_string();
                if end < line.len() {
                    preview.push_str("…");
                }
                state.matches.push(SearchMatch {
                    path: relative.clone(),
                    line_number: index + 1,
                    line: preview,
                });
                if state.matches.len() >= MAX_SEARCH_MATCHES {
                    state.truncated = true;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    let mut state = SearchState {
        query,
        folded_query: query.to_lowercase(),
        case_sensitive,
        matches: Vec::new(),
        files_seen: 0,
        files_searched: 0,
        files_skipped: 0,
        entries_seen: 0,
        bytes_searched: 0,
        truncated: false,
    };
    visit(&root, &directory, 0, &mut state)?;
    Ok(SearchOutcome {
        matches: state.matches,
        files_searched: state.files_searched,
        files_skipped: state.files_skipped,
        truncated: state.truncated,
    })
}

pub fn read_file(workspace: &str, relative_path: &str) -> Result<String, String> {
    let root = workspace_root(workspace)?;
    let components = safe_components(relative_path, false)?;
    let path = checked_existing(&root, &components)?;
    let metadata = fs::metadata(&path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("只能读取普通文件".to_string());
    }
    if metadata.len() > MAX_TEXT_BYTES {
        return Err("文件超过 1 MiB 安全上限；Agent 不读取超大文件".to_string());
    }
    fs::read_to_string(path)
        .map_err(|error| format!("文件不是可读取的 UTF-8 文本，或读取失败：{error}"))
}

pub fn write_file(
    workspace: &str,
    relative_path: &str,
    content: &str,
    allow_overwrite: bool,
) -> Result<WriteOutcome, String> {
    if content.len() as u64 > MAX_TEXT_BYTES {
        return Err("单个文件写入上限为 1 MiB".to_string());
    }
    let root = workspace_root(workspace)?;
    let components = safe_components(relative_path, false)?;
    let (file_name, parent_components) = components
        .split_last()
        .ok_or_else(|| "文件路径不能为空".to_string())?;
    let mut parent = root.clone();
    for component in parent_components {
        parent.push(component);
        match fs::symlink_metadata(&parent) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("Agent 不会在符号链接目录中写入文件".to_string());
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err("文件路径中的父级不是目录".to_string())
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&parent)
                    .map_err(|error| format!("创建工作区子目录失败：{error}"))?;
            }
            Err(error) => return Err(format!("检查父目录失败：{error}")),
        }
    }
    let canonical_parent = parent.canonicalize().map_err(|error| error.to_string())?;
    if !canonical_parent.starts_with(&root) {
        return Err("目标目录超出所选工作文件夹".to_string());
    }
    let target = canonical_parent.join(file_name);
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err("Agent 不会覆盖符号链接".to_string())
        }
        Ok(metadata) if !metadata.is_file() => return Err("目标路径不是普通文件".to_string()),
        Ok(_) if !allow_overwrite => {
            return Ok(WriteOutcome {
                written: false,
                requires_confirmation: true,
                cancelled: false,
            })
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("检查目标文件失败：{error}")),
    }
    let mut options = OpenOptions::new();
    options.write(true);
    if allow_overwrite {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = options.open(&target).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            "目标文件刚刚出现；请再次确认是否覆盖".to_string()
        } else {
            format!("创建或写入文件失败：{error}")
        }
    })?;
    file.write_all(content.as_bytes())
        .map_err(|error| format!("写入文件失败：{error}"))?;
    file.flush()
        .map_err(|error| format!("刷新文件失败：{error}"))?;
    Ok(WriteOutcome {
        written: true,
        requires_confirmation: false,
        cancelled: false,
    })
}

pub fn edit_file(
    workspace: &str,
    relative_path: &str,
    old_text: &str,
    new_text: &str,
    allow_edit: bool,
) -> Result<EditFileOutcome, String> {
    if old_text.is_empty() {
        return Err("old_text 不能为空；需要至少一个精确匹配片段".to_string());
    }
    if old_text.len() as u64 > MAX_TEXT_BYTES || new_text.len() as u64 > MAX_TEXT_BYTES {
        return Err("old_text 和 new_text 的上限均为 1 MiB".to_string());
    }

    let root = workspace_root(workspace)?;
    let components = safe_components(relative_path, false)?;
    let target = checked_existing(&root, &components)?;
    let metadata =
        fs::symlink_metadata(&target).map_err(|error| format!("检查待编辑文件失败：{error}"))?;
    if metadata.file_type().is_symlink() {
        return Err("Agent 不编辑符号链接".to_string());
    }
    if !metadata.is_file() {
        return Err("edit_file 只允许修改工作区内的普通文件".to_string());
    }
    if metadata.len() > MAX_TEXT_BYTES {
        return Err("文件超过 1 MiB 安全上限；Agent 不编辑超大文件".to_string());
    }
    let original = fs::read_to_string(&target)
        .map_err(|error| format!("文件不是可读取的 UTF-8 文本，或读取失败：{error}"))?;
    let start = original
        .find(old_text)
        .ok_or_else(|| "old_text 未在文件中找到；文件未修改".to_string())?;
    let first_character_bytes = original[start..]
        .chars()
        .next()
        .map(char::len_utf8)
        .ok_or_else(|| "old_text 未在文件中找到；文件未修改".to_string())?;
    if original[start + first_character_bytes..].contains(old_text) {
        return Err("old_text 在文件中出现多次，无法确定唯一修改位置；文件未修改".to_string());
    }

    let end = start + old_text.len();
    let mut updated = String::with_capacity(original.len() - old_text.len() + new_text.len());
    updated.push_str(&original[..start]);
    updated.push_str(new_text);
    updated.push_str(&original[end..]);
    if updated.len() as u64 > MAX_TEXT_BYTES {
        return Err("编辑后文件超过 1 MiB 安全上限；文件未修改".to_string());
    }
    if updated == original {
        return Ok(EditFileOutcome {
            edited: false,
            requires_confirmation: false,
            cancelled: false,
        });
    }
    if !allow_edit {
        return Ok(EditFileOutcome {
            edited: false,
            requires_confirmation: true,
            cancelled: false,
        });
    }

    let parent = target
        .parent()
        .ok_or_else(|| "无法解析待编辑文件的工作区父目录".to_string())?;
    let mut staged = NamedTempFile::new_in(parent)
        .map_err(|error| format!("创建原子写入临时文件失败：{error}"))?;
    staged
        .write_all(updated.as_bytes())
        .map_err(|error| format!("写入原子编辑临时文件失败：{error}"))?;
    staged
        .as_file()
        .set_permissions(metadata.permissions())
        .map_err(|error| format!("保留原文件权限失败：{error}"))?;
    staged
        .as_file()
        .sync_all()
        .map_err(|error| format!("同步原子编辑临时文件失败：{error}"))?;
    staged
        .persist(&target)
        .map_err(|error| format!("原子替换工作区文件失败：{}", error.error))?;

    Ok(EditFileOutcome {
        edited: true,
        requires_confirmation: false,
        cancelled: false,
    })
}

pub fn create_folder(workspace: &str, relative_path: &str) -> Result<CreateFolderOutcome, String> {
    let root = workspace_root(workspace)?;
    let components = safe_components(relative_path, false)?;
    let mut current = root.clone();
    let mut first_missing = None;

    for (index, component) in components.iter().enumerate() {
        let next = current.join(component);
        match fs::symlink_metadata(&next) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("Agent 不跟随工作文件夹中的符号链接".to_string());
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err("创建文件夹的目标路径中存在非文件夹项目".to_string());
            }
            Ok(_) => {
                let canonical = next
                    .canonicalize()
                    .map_err(|error| format!("检查工作区文件夹失败：{error}"))?;
                if !canonical.starts_with(&root) {
                    return Err("创建文件夹的路径超出所选工作文件夹".to_string());
                }
                current = canonical;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                first_missing = Some(index);
                break;
            }
            Err(error) => return Err(format!("检查工作区文件夹失败：{error}")),
        }
    }

    let Some(first_missing) = first_missing else {
        return Ok(CreateFolderOutcome { created: false });
    };

    let mut created = false;
    for component in &components[first_missing..] {
        let next = current.join(component);
        let created_here = match fs::create_dir(&next) {
            Ok(()) => {
                created = true;
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(format!("创建工作区文件夹失败：{error}")),
        };
        let metadata = fs::symlink_metadata(&next)
            .map_err(|error| format!("检查新工作区文件夹失败：{error}"))?;
        if metadata.file_type().is_symlink() {
            return Err("Agent 不跟随工作文件夹中的符号链接".to_string());
        }
        if !metadata.is_dir() {
            return Err("创建文件夹的目标路径中存在非文件夹项目".to_string());
        }
        let canonical = next
            .canonicalize()
            .map_err(|error| format!("检查新工作区文件夹失败：{error}"))?;
        if !canonical.starts_with(&root) {
            if created_here {
                let _ = fs::remove_dir(&next);
            }
            return Err("创建文件夹的路径超出所选工作文件夹".to_string());
        }
        current = canonical;
    }

    Ok(CreateFolderOutcome { created })
}

pub fn delete_file(workspace: &str, relative_path: &str) -> Result<(), String> {
    let root = workspace_root(workspace)?;
    let components = safe_components(relative_path, false)?;
    let target = checked_existing(&root, &components)?;
    let metadata = fs::symlink_metadata(&target).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Agent 仅允许删除工作文件夹内的普通文件；不允许删除链接或目录".to_string());
    }
    fs::remove_file(target).map_err(|error| format!("删除文件失败：{error}"))
}

pub fn run_command(
    workspace: &str,
    program: &str,
    args: &[String],
) -> Result<CommandOutput, String> {
    if program.is_empty()
        || program.len() > 256
        || program.contains('/')
        || program.contains('\\')
        || program.contains(':')
        || program.chars().any(char::is_control)
        || program == "."
        || program == ".."
        || args.len() > 100
        || args
            .iter()
            .any(|arg| arg.len() > MAX_COMMAND_BYTES || arg.contains('\0'))
        || args.iter().map(String::len).sum::<usize>() > MAX_COMMAND_BYTES
    {
        return Err("命令无效或超出安全长度限制".to_string());
    }
    let root = workspace_root(workspace)?;
    let mut child = Command::new(program)
        .args(args)
        .current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("启动命令失败：{error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法捕获命令标准输出".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法捕获命令错误输出".to_string())?;
    let stdout_receiver = capture_stream(stdout);
    let stderr_receiver = capture_stream(stderr);
    let started = Instant::now();
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status, false),
            Ok(None) if started.elapsed() >= MAX_COMMAND_RUNTIME => {
                let _ = child.kill();
                let status = child
                    .wait()
                    .map_err(|error| format!("终止超时命令失败：{error}"))?;
                break (status, true);
            }
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("检查命令运行状态失败：{error}"));
            }
        }
    };
    let stdout = collect_stream(stdout_receiver, "标准输出")?;
    let mut stderr = collect_stream(stderr_receiver, "错误输出")?;
    if timed_out {
        stderr.push_str("\n命令超过 120 秒时限；已终止直接启动的进程。");
    }
    Ok(CommandOutput {
        success: status.success() && !timed_out,
        timed_out,
        cancelled: false,
        exit_code: status.code(),
        stdout,
        stderr,
    })
}

fn capture_stream<R: Read + Send + 'static>(
    mut stream: R,
) -> Receiver<Result<(Vec<u8>, bool), String>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut captured = Vec::with_capacity(MAX_TOOL_OUTPUT_BYTES);
        let mut truncated = false;
        let mut buffer = [0u8; 8192];
        let result = loop {
            match stream.read(&mut buffer) {
                Ok(0) => break Ok((captured, truncated)),
                Ok(read) => {
                    let remaining = MAX_TOOL_OUTPUT_BYTES.saturating_sub(captured.len());
                    let kept = read.min(remaining);
                    captured.extend_from_slice(&buffer[..kept]);
                    truncated |= kept < read;
                }
                Err(error) => break Err(format!("读取命令输出失败：{error}")),
            }
        };
        let _ = sender.send(result);
    });
    receiver
}

fn collect_stream(
    receiver: Receiver<Result<(Vec<u8>, bool), String>>,
    label: &str,
) -> Result<String, String> {
    let (bytes, was_truncated) = receiver
        .recv_timeout(COMMAND_OUTPUT_DRAIN_TIMEOUT)
        .map_err(|error| format!("等待命令{label}结束失败：{error}"))??;
    let decoded = String::from_utf8_lossy(&bytes);
    let mut output = truncate_utf8(&decoded, MAX_TOOL_OUTPUT_BYTES);
    if was_truncated {
        output.push_str("\n…（超过 32 KiB 的后续输出已丢弃）");
    }
    Ok(output)
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…（输出已截断）", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::{
        capture_stream, collect_stream, create_folder, delete_file, edit_file, list_files,
        read_file, run_command, search_files, write_file, MAX_SEARCH_FILES, MAX_SEARCH_MATCHES,
        MAX_TOOL_OUTPUT_BYTES,
    };
    use std::{fs, io::Cursor, path::Path};
    use tempfile::tempdir;

    fn make_workspace(root: &Path) -> std::path::PathBuf {
        let workspace = root.join("workspace");
        fs::create_dir(&workspace).unwrap();
        workspace
    }

    #[test]
    fn file_tools_reject_parent_and_absolute_paths() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        fs::write(temp.path().join("outside.txt"), "outside").unwrap();
        let root = workspace.to_string_lossy();
        assert!(read_file(&root, "../outside.txt").is_err());
        assert!(write_file(&root, "../outside.txt", "changed", false).is_err());
        assert!(delete_file(&root, "../outside.txt").is_err());
        let absolute = temp
            .path()
            .join("outside.txt")
            .to_string_lossy()
            .into_owned();
        assert!(read_file(&root, &absolute).is_err());
        assert_eq!(
            fs::read_to_string(temp.path().join("outside.txt")).unwrap(),
            "outside"
        );
    }

    #[test]
    fn existing_file_is_not_overwritten_without_confirmation() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        fs::write(workspace.join("note.txt"), "original").unwrap();
        let root = workspace.to_string_lossy();

        let outcome = write_file(&root, "note.txt", "replacement", false).unwrap();
        assert!(!outcome.written);
        assert!(outcome.requires_confirmation);
        assert_eq!(read_file(&root, "note.txt").unwrap(), "original");

        let outcome = write_file(&root, "note.txt", "replacement", true).unwrap();
        assert!(outcome.written);
        assert!(!outcome.requires_confirmation);
        assert_eq!(read_file(&root, "note.txt").unwrap(), "replacement");
    }

    #[test]
    fn writing_a_file_creates_missing_parent_directories_inside_the_workspace() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        let root = workspace.to_string_lossy();

        let outcome = write_file(&root, "notes/drafts/README.txt", "created", false).unwrap();

        assert!(outcome.written);
        assert!(!outcome.requires_confirmation);
        assert!(workspace.join("notes/drafts").is_dir());
        assert_eq!(
            read_file(&root, "notes/drafts/README.txt").unwrap(),
            "created"
        );
    }

    #[test]
    fn create_folder_creates_an_empty_nested_directory() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        let root = workspace.to_string_lossy();

        let outcome = create_folder(&root, "notes/drafts").unwrap();

        assert!(outcome.created);
        let folder = workspace.join("notes/drafts");
        assert!(folder.is_dir());
        assert_eq!(fs::read_dir(folder).unwrap().count(), 0);
    }

    #[test]
    fn create_folder_is_idempotent_and_rejects_a_file_at_the_target() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        let root = workspace.to_string_lossy();

        assert!(create_folder(&root, "already-here").unwrap().created);
        assert!(!create_folder(&root, "already-here").unwrap().created);
        fs::write(workspace.join("ordinary-file"), "not a folder").unwrap();
        assert!(create_folder(&root, "ordinary-file").is_err());
    }

    #[test]
    fn create_folder_rejects_parent_traversal_and_absolute_paths() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        let root = workspace.to_string_lossy();
        let outside = temp.path().join("outside");

        assert!(create_folder(&root, "../outside/escape").is_err());
        assert!(create_folder(&root, &outside.to_string_lossy()).is_err());
        assert!(!outside.exists());
    }

    #[cfg(unix)]
    #[test]
    fn create_folder_rejects_a_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, workspace.join("linked-outside")).unwrap();
        let root = workspace.to_string_lossy();

        assert!(create_folder(&root, "linked-outside/escape").is_err());
        assert!(!outside.join("escape").exists());
    }

    #[test]
    fn edit_file_requires_confirmation_then_replaces_one_exact_match() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        fs::write(workspace.join("note.txt"), "before old text after").unwrap();
        let root = workspace.to_string_lossy();

        let preview = edit_file(&root, "note.txt", "old text", "new text", false).unwrap();
        assert!(!preview.edited);
        assert!(preview.requires_confirmation);
        assert!(!preview.cancelled);
        assert_eq!(
            read_file(&root, "note.txt").unwrap(),
            "before old text after"
        );

        let approved = edit_file(&root, "note.txt", "old text", "new text", true).unwrap();
        assert!(approved.edited);
        assert!(!approved.requires_confirmation);
        assert_eq!(
            read_file(&root, "note.txt").unwrap(),
            "before new text after"
        );
    }

    #[test]
    fn edit_file_rejects_missing_absent_duplicate_overlapping_and_empty_matches() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        let root = workspace.to_string_lossy();

        assert!(edit_file(&root, "missing.txt", "old", "new", true).is_err());
        fs::write(workspace.join("note.txt"), "alpha old omega").unwrap();
        assert!(edit_file(&root, "note.txt", "absent", "new", true).is_err());
        assert!(edit_file(&root, "note.txt", "", "new", true).is_err());
        fs::write(workspace.join("note.txt"), "same | same").unwrap();
        assert!(edit_file(&root, "note.txt", "same", "new", true).is_err());
        fs::write(workspace.join("note.txt"), "aaa").unwrap();
        assert!(edit_file(&root, "note.txt", "aa", "new", true).is_err());
        assert_eq!(read_file(&root, "note.txt").unwrap(), "aaa");
    }

    #[test]
    fn edit_file_rejects_parent_traversal_and_absolute_paths() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, "outside").unwrap();
        let root = workspace.to_string_lossy();

        assert!(edit_file(&root, "../outside.txt", "outside", "changed", true).is_err());
        assert!(edit_file(
            &root,
            &outside.to_string_lossy(),
            "outside",
            "changed",
            true
        )
        .is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside");
    }

    #[cfg(unix)]
    #[test]
    fn edit_file_rejects_a_symlink_escape() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, "outside").unwrap();
        symlink(&outside, workspace.join("linked.txt")).unwrap();
        let root = workspace.to_string_lossy();

        assert!(edit_file(&root, "linked.txt", "outside", "changed", true).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside");
    }

    #[test]
    fn listing_hides_git_metadata_and_delete_refuses_directories() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        fs::create_dir(workspace.join(".git")).unwrap();
        fs::write(workspace.join(".git/config"), "private").unwrap();
        fs::create_dir(workspace.join("folder")).unwrap();
        let root = workspace.to_string_lossy();

        let entries = list_files(&root, ".").unwrap();
        assert!(entries.iter().any(|entry| entry.path == "folder"));
        assert!(entries.iter().all(|entry| !entry.path.contains(".git")));
        assert!(delete_file(&root, "folder").is_err());
    }

    #[test]
    fn search_is_case_insensitive_by_default_and_honors_its_start_directory() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        fs::create_dir(workspace.join("src")).unwrap();
        fs::create_dir(workspace.join("docs")).unwrap();
        fs::write(
            workspace.join("src/main.rs"),
            "fn main() {}\nTODO: Fix this\n",
        )
        .unwrap();
        fs::write(workspace.join("docs/readme.md"), "todo: document this\n").unwrap();
        let root = workspace.to_string_lossy();

        let results = search_files(&root, "src", "todo", false).unwrap();
        assert_eq!(results.files_searched, 1);
        assert_eq!(results.matches.len(), 1);
        assert_eq!(results.matches[0].path, "src/main.rs");
        assert_eq!(results.matches[0].line_number, 2);
        assert!(results.matches[0].line.contains("TODO"));
        assert!(search_files(&root, "src", "todo", true)
            .unwrap()
            .matches
            .is_empty());
    }

    #[test]
    fn search_rejects_parent_paths_and_skips_git_metadata_and_binary_files() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        fs::create_dir(workspace.join(".git")).unwrap();
        fs::write(workspace.join(".git/config"), "needle in git").unwrap();
        fs::write(workspace.join("binary.dat"), b"needle\0secret").unwrap();
        let root = workspace.to_string_lossy();

        assert!(search_files(&root, "../", "needle", false).is_err());
        let results = search_files(&root, ".", "needle", false).unwrap();
        assert!(results.matches.is_empty());
        assert_eq!(results.files_skipped, 1);
    }

    #[cfg(unix)]
    #[test]
    fn search_skips_symlink_directories_and_treats_query_as_literal_text() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "needle").unwrap();
        symlink(&outside, workspace.join("linked")).unwrap();
        fs::write(workspace.join("patterns.txt"), "a.*b\naxb\n").unwrap();
        let root = workspace.to_string_lossy();

        assert!(search_files(&root, "linked", "needle", false).is_err());
        let result = search_files(&root, ".", "a.*b", false).unwrap();
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].path, "patterns.txt");
    }

    #[test]
    fn search_limits_match_count_and_marks_the_result_as_truncated() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        let contents = std::iter::repeat_n("match", MAX_SEARCH_MATCHES + 5)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(workspace.join("many.txt"), contents).unwrap();
        let root = workspace.to_string_lossy();

        let results = search_files(&root, ".", "match", false).unwrap();
        assert_eq!(results.matches.len(), MAX_SEARCH_MATCHES);
        assert!(results.truncated);
    }

    #[test]
    fn search_limits_files_and_marks_skipped_build_directories_incomplete() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        for index in 0..=MAX_SEARCH_FILES {
            fs::write(workspace.join(format!("file-{index:04}.txt")), "no hit").unwrap();
        }
        let root = workspace.to_string_lossy();

        let results = search_files(&root, ".", "not-found", false).unwrap();
        assert_eq!(results.files_searched, MAX_SEARCH_FILES);
        assert!(results.truncated);

        let project_dir = workspace.join("small-project");
        fs::create_dir(&project_dir).unwrap();
        let build_dir = project_dir.join("target");
        fs::create_dir(&build_dir).unwrap();
        fs::write(build_dir.join("generated.txt"), "needle").unwrap();
        let build_results = search_files(&root, "small-project/target", "needle", false).unwrap();
        assert_eq!(build_results.matches.len(), 1);
        let workspace_results = search_files(&root, "small-project", "needle", false).unwrap();
        assert!(workspace_results.matches.is_empty());
        assert!(workspace_results.truncated);
    }

    #[test]
    fn search_skips_files_larger_than_one_mib_and_truncates_long_previews() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        fs::write(workspace.join("large.txt"), vec![b'x'; 1024 * 1024 + 1]).unwrap();
        fs::write(
            workspace.join("line.txt"),
            format!("match{}", "x".repeat(500)),
        )
        .unwrap();
        let root = workspace.to_string_lossy();

        let results = search_files(&root, ".", "match", false).unwrap();
        assert_eq!(results.files_skipped, 1);
        assert_eq!(results.matches.len(), 1);
        assert!(results.matches[0].line.ends_with('…'));
        assert!(results.matches[0].line.len() <= 243);
    }

    #[cfg(unix)]
    #[test]
    fn file_tools_do_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        let outside = temp.path().join("outside.txt");
        fs::write(&outside, "outside").unwrap();
        symlink(&outside, workspace.join("linked.txt")).unwrap();
        let root = workspace.to_string_lossy();

        assert!(read_file(&root, "linked.txt").is_err());
        assert!(write_file(&root, "linked.txt", "changed", true).is_err());
        assert!(delete_file(&root, "linked.txt").is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside");
    }

    #[test]
    fn command_tool_rejects_executable_paths_before_spawning() {
        let temp = tempdir().unwrap();
        let workspace = make_workspace(temp.path());
        let root = workspace.to_string_lossy();
        assert!(run_command(&root, "../python", &[]).is_err());
        assert!(run_command(&root, "python", &["bad\0arg".to_string()]).is_err());
    }

    #[test]
    fn command_output_capture_discards_bytes_after_the_limit() {
        let input = vec![b'x'; MAX_TOOL_OUTPUT_BYTES + 2048];
        let receiver = capture_stream(Cursor::new(input));
        let output = collect_stream(receiver, "标准输出").unwrap();
        assert!(output.len() <= MAX_TOOL_OUTPUT_BYTES + 64);
        assert!(output.contains("超过 32 KiB"));
    }
}
