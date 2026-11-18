use serde::Serialize;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

const MAX_TEXT_BYTES: u64 = 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_TOOL_OUTPUT_BYTES: usize = 32 * 1024;
const MAX_LIST_ENTRIES: usize = 500;
const MAX_LIST_DEPTH: usize = 6;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceFile {
    pub path: String,
    pub kind: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteOutcome {
    pub written: bool,
    pub requires_confirmation: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandOutput {
    pub success: bool,
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
    })
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
    let output = Command::new(program)
        .args(args)
        .current_dir(&root)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("启动命令失败：{error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(CommandOutput {
        success: output.status.success(),
        exit_code: output.status.code(),
        stdout: truncate_utf8(&stdout, MAX_TOOL_OUTPUT_BYTES),
        stderr: truncate_utf8(&stderr, MAX_TOOL_OUTPUT_BYTES),
    })
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
