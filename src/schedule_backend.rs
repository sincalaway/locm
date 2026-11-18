use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager};

const FILE_NAME: &str = "scheduled-tasks.json";
const MAX_TASKS: usize = 100;
const MAX_TITLE_BYTES: usize = 200;
const MAX_PROMPT_BYTES: usize = 32 * 1024;
const MAX_HISTORY: usize = 100;
const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ID_BYTES: usize = 128;
static STORE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledTask {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub scheduled_at_ms: i64,
    pub recurrence: Recurrence,
    pub enabled: bool,
    pub next_run_at_ms: Option<i64>,
    pub last_run_at_ms: Option<i64>,
    pub last_status: Option<String>,
    pub last_summary: Option<String>,
    pub history: Vec<ExecutionRecord>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Recurrence {
    Once,
    Hourly,
    Daily,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionRecord {
    pub run_at_ms: i64,
    pub status: String,
    pub summary: String,
}

pub fn list(app: &AppHandle) -> Result<Vec<ScheduledTask>, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "定时任务存储锁定失败".to_string())?;
    load_unlocked(app)
}

pub fn skip_missed_runs(app: &AppHandle) -> Result<(), String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "定时任务存储锁定失败".to_string())?;
    let now = now_ms();
    let mut tasks = load_unlocked(app)?;
    let mut changed = false;
    for task in &mut tasks {
        let Some(due_at) = task.next_run_at_ms.filter(|due_at| *due_at <= now) else {
            continue;
        };
        let summary = "应用关闭期间错过计划时间；按设置跳过，未补跑".to_string();
        task.last_run_at_ms = Some(due_at);
        task.last_status = Some("skipped".to_string());
        task.last_summary = Some(summary.clone());
        task.history.push(ExecutionRecord {
            run_at_ms: due_at,
            status: "skipped".to_string(),
            summary,
        });
        if task.history.len() > MAX_HISTORY {
            let excess = task.history.len() - MAX_HISTORY;
            task.history.drain(0..excess);
        }
        match task.recurrence {
            Recurrence::Once => {
                task.enabled = false;
                task.next_run_at_ms = None;
            }
            recurrence => {
                let interval = interval_ms(recurrence);
                let elapsed = now.saturating_sub(due_at);
                let periods = elapsed.div_euclid(interval).saturating_add(1);
                task.next_run_at_ms = Some(due_at.saturating_add(periods.saturating_mul(interval)));
            }
        }
        changed = true;
    }
    if changed {
        save_unlocked(app, &tasks)?;
    }
    Ok(())
}

pub fn create(
    app: &AppHandle,
    title: String,
    prompt: String,
    scheduled_at_ms: i64,
    recurrence: Recurrence,
) -> Result<ScheduledTask, String> {
    validate_input(&title, &prompt, scheduled_at_ms)?;
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "定时任务存储锁定失败".to_string())?;
    let mut tasks = load_unlocked(app)?;
    if tasks.len() >= MAX_TASKS {
        return Err("定时任务最多保存 100 个".to_string());
    }
    let now = now_ms();
    if scheduled_at_ms <= now {
        return Err("计划时间必须晚于当前时间".to_string());
    }
    let task = ScheduledTask {
        id: format!("{}-{}", now, next_id()),
        title,
        prompt,
        scheduled_at_ms,
        recurrence,
        enabled: true,
        next_run_at_ms: Some(scheduled_at_ms),
        last_run_at_ms: None,
        last_status: None,
        last_summary: None,
        history: Vec::new(),
    };
    tasks.push(task.clone());
    save_unlocked(app, &tasks)?;
    Ok(task)
}

pub fn set_enabled(app: &AppHandle, id: &str, enabled: bool) -> Result<ScheduledTask, String> {
    validate_id(id)?;
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "定时任务存储锁定失败".to_string())?;
    let mut tasks = load_unlocked(app)?;
    let updated = {
        let task = tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or_else(|| "定时任务不存在".to_string())?;
        if enabled && task.recurrence == Recurrence::Once && task.last_run_at_ms.is_some() {
            return Err("一次性任务已结束；如需再次运行，请创建新任务".to_string());
        }
        task.enabled = enabled;
        if enabled && task.next_run_at_ms.is_none() {
            task.next_run_at_ms = Some(match task.recurrence {
                Recurrence::Once => task.scheduled_at_ms,
                _ => now_ms().saturating_add(interval_ms(task.recurrence)),
            });
        }
        task.clone()
    };
    save_unlocked(app, &tasks)?;
    Ok(updated)
}

pub fn delete(app: &AppHandle, id: &str) -> Result<(), String> {
    validate_id(id)?;
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "定时任务存储锁定失败".to_string())?;
    let mut tasks = load_unlocked(app)?;
    let before = tasks.len();
    tasks.retain(|task| task.id != id);
    if tasks.len() == before {
        return Err("定时任务不存在".to_string());
    }
    save_unlocked(app, &tasks)
}

pub fn start_worker<F>(app: AppHandle, stop: Arc<AtomicBool>, execute: F) -> thread::JoinHandle<()>
where
    F: Fn(&AppHandle, &str) -> Result<String, String> + Send + Sync + 'static,
{
    let execute = Arc::new(execute);
    thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let now = now_ms();
            let due = match due_tasks(&app, now) {
                Ok(tasks) => tasks,
                Err(_) => Vec::new(),
            };
            for (id, prompt) in due {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let result = execute(&app, &prompt);
                let (status, summary) = match result {
                    Ok(text) => ("success".to_string(), truncate_summary(text)),
                    Err(error) => ("failed".to_string(), truncate_summary(error)),
                };
                let _ = record_execution(&app, &id, now_ms(), status, summary);
            }
            for _ in 0..30 {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    })
}

fn due_tasks(app: &AppHandle, now: i64) -> Result<Vec<(String, String)>, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "定时任务存储锁定失败".to_string())?;
    Ok(load_unlocked(app)?
        .into_iter()
        .filter(|task| task.enabled && task.next_run_at_ms.is_some_and(|at| at <= now))
        .map(|task| (task.id, task.prompt))
        .collect())
}

fn record_execution(
    app: &AppHandle,
    id: &str,
    run_at: i64,
    status: String,
    summary: String,
) -> Result<(), String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "定时任务存储锁定失败".to_string())?;
    let mut tasks = load_unlocked(app)?;
    let task = match tasks.iter_mut().find(|task| task.id == id) {
        Some(task) => task,
        None => return Ok(()),
    };
    task.last_run_at_ms = Some(run_at);
    task.last_status = Some(status.clone());
    task.last_summary = Some(summary.clone());
    task.history.push(ExecutionRecord {
        run_at_ms: run_at,
        status,
        summary,
    });
    if task.history.len() > MAX_HISTORY {
        let excess = task.history.len() - MAX_HISTORY;
        task.history.drain(0..excess);
    }
    match task.recurrence {
        Recurrence::Once => {
            task.enabled = false;
            task.next_run_at_ms = None;
        }
        recurrence => {
            let interval = interval_ms(recurrence);
            let mut next = task
                .next_run_at_ms
                .unwrap_or(run_at)
                .saturating_add(interval);
            while next <= run_at {
                next = next.saturating_add(interval);
            }
            task.next_run_at_ms = Some(next);
        }
    }
    save_unlocked(app, &tasks)
}

fn load_unlocked(app: &AppHandle) -> Result<Vec<ScheduledTask>, String> {
    let path = data_path(app)?;
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("读取定时任务失败：{error}")),
    };
    if text.len() > MAX_FILE_BYTES {
        return Err("定时任务文件超过 16 MiB 安全上限".to_string());
    }
    let tasks: Vec<ScheduledTask> =
        serde_json::from_str(&text).map_err(|error| format!("定时任务文件格式无效：{error}"))?;
    validate_tasks(&tasks)?;
    Ok(tasks)
}

fn save_unlocked(app: &AppHandle, tasks: &[ScheduledTask]) -> Result<(), String> {
    validate_tasks(tasks)?;
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法确定应用数据目录：{error}"))?;
    fs::create_dir_all(&directory).map_err(|error| format!("无法创建应用数据目录：{error}"))?;
    let path = directory.join(FILE_NAME);
    let temporary = directory.join(format!("{FILE_NAME}.tmp"));
    let backup = directory.join(format!("{FILE_NAME}.bak"));
    let bytes = serde_json::to_vec_pretty(tasks).map_err(|error| error.to_string())?;
    fs::write(&temporary, bytes).map_err(|error| format!("写入定时任务失败：{error}"))?;
    if cfg!(windows) && path.exists() {
        if backup.exists() {
            fs::remove_file(&backup).map_err(|error| format!("清理旧任务备份失败：{error}"))?;
        }
        fs::rename(&path, &backup).map_err(|error| format!("暂存旧任务文件失败：{error}"))?;
        if let Err(error) = fs::rename(&temporary, &path) {
            let _ = fs::rename(&backup, &path);
            let _ = fs::remove_file(&temporary);
            return Err(format!("保存定时任务失败：{error}"));
        }
        let _ = fs::remove_file(backup);
    } else if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("保存定时任务失败：{error}"));
    }
    Ok(())
}

fn data_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法确定应用数据目录：{error}"))?
        .join(FILE_NAME))
}
fn validate_tasks(tasks: &[ScheduledTask]) -> Result<(), String> {
    if tasks.len() > MAX_TASKS {
        return Err("定时任务最多保存 100 个".to_string());
    }
    for task in tasks {
        validate_id(&task.id)?;
        validate_input(&task.title, &task.prompt, task.scheduled_at_ms)?;
        if task.history.len() > MAX_HISTORY {
            return Err("单个任务历史最多 100 条".to_string());
        }
    }
    Ok(())
}
fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        Err("定时任务 ID 无效".to_string())
    } else {
        Ok(())
    }
}
fn validate_input(title: &str, prompt: &str, scheduled_at_ms: i64) -> Result<(), String> {
    if title.trim().is_empty() || title.len() > MAX_TITLE_BYTES {
        return Err("标题不能为空且不得超过 200 字节".to_string());
    }
    if prompt.trim().is_empty() || prompt.len() > MAX_PROMPT_BYTES {
        return Err("提示内容不能为空且不得超过 32 KiB".to_string());
    }
    if scheduled_at_ms <= 0 {
        return Err("计划时间必须是有效的 epoch 毫秒".to_string());
    }
    Ok(())
}
fn interval_ms(recurrence: Recurrence) -> i64 {
    match recurrence {
        Recurrence::Hourly => 3_600_000,
        Recurrence::Daily => 86_400_000,
        Recurrence::Once => 0,
    }
}
fn truncate_summary(value: String) -> String {
    value.chars().take(2_000).collect()
}
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
fn next_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recurrence_intervals_are_fixed() {
        assert_eq!(interval_ms(Recurrence::Hourly), 3_600_000);
        assert_eq!(interval_ms(Recurrence::Daily), 86_400_000);
    }
    #[test]
    fn summary_is_bounded() {
        assert_eq!(truncate_summary("x".repeat(3_000)).len(), 2_000);
    }
}
