use reqwest::{blocking::Client, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
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
const MAX_CHANNELS: usize = 20;
const MAX_CHANNEL_FIELD_BYTES: usize = 4096;
const MAX_PUSH_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_INTERVAL_MINUTES: u32 = 525_600;
static STORE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledTask {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub scheduled_at_ms: i64,
    pub recurrence: Recurrence,
    #[serde(default)]
    pub interval_minutes: Option<u32>,
    #[serde(default)]
    pub notify: bool,
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
    Weekly,
    Interval,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionRecord {
    pub run_at_ms: i64,
    pub status: String,
    pub summary: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PushChannelKind {
    Telegram,
    ServerChan,
    PushPlus,
    DingTalk,
    Feishu,
    Bark,
    QqWebhook,
    Webhook,
}

/// Credentials are intentionally held only in memory: this value is never written to task storage.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushChannelConfig {
    pub id: String,
    pub name: String,
    pub kind: PushChannelKind,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushChannelSummary {
    pub id: String,
    pub name: String,
    pub kind: PushChannelKind,
}

#[derive(Default)]
pub struct PushChannelState {
    channels: Mutex<Vec<PushChannelConfig>>,
}

impl PushChannelState {
    pub fn save_channel(
        &self,
        channel: PushChannelConfig,
    ) -> Result<Vec<PushChannelSummary>, String> {
        validate_channel(&channel)?;
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "推送渠道状态锁定失败".to_string())?;
        if let Some(existing) = channels.iter_mut().find(|item| item.id == channel.id) {
            *existing = channel;
        } else {
            if channels.len() >= MAX_CHANNELS {
                return Err(format!("推送渠道最多配置 {MAX_CHANNELS} 个"));
            }
            channels.push(channel);
        }
        Ok(channels
            .iter()
            .map(|channel| PushChannelSummary {
                id: channel.id.clone(),
                name: channel.name.clone(),
                kind: channel.kind,
            })
            .collect())
    }

    pub fn delete_channel(&self, id: &str) -> Result<Vec<PushChannelSummary>, String> {
        validate_id(id)?;
        let mut channels = self
            .channels
            .lock()
            .map_err(|_| "推送渠道状态锁定失败".to_string())?;
        let before = channels.len();
        channels.retain(|channel| channel.id != id);
        if channels.len() == before {
            return Err("推送渠道不存在".to_string());
        }
        Ok(channels
            .iter()
            .map(|channel| PushChannelSummary {
                id: channel.id.clone(),
                name: channel.name.clone(),
                kind: channel.kind,
            })
            .collect())
    }

    pub fn summaries(&self) -> Result<Vec<PushChannelSummary>, String> {
        let channels = self
            .channels
            .lock()
            .map_err(|_| "推送渠道状态锁定失败".to_string())?;
        Ok(channels
            .iter()
            .map(|channel| PushChannelSummary {
                id: channel.id.clone(),
                name: channel.name.clone(),
                kind: channel.kind,
            })
            .collect())
    }
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
        trim_history(&mut task.history);
        match task.recurrence {
            Recurrence::Once => {
                task.enabled = false;
                task.next_run_at_ms = None;
            }
            recurrence => {
                let interval = interval_ms(recurrence, task.interval_minutes);
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
    interval_minutes: Option<u32>,
    notify: bool,
) -> Result<ScheduledTask, String> {
    validate_input(&title, &prompt, scheduled_at_ms)?;
    validate_recurrence(recurrence, interval_minutes)?;
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
        interval_minutes,
        notify,
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
                recurrence => {
                    now_ms().saturating_add(interval_ms(recurrence, task.interval_minutes))
                }
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

pub fn start_worker<F, N>(
    app: AppHandle,
    stop: Arc<AtomicBool>,
    execute: F,
    notify: N,
) -> thread::JoinHandle<()>
where
    F: Fn(&AppHandle, &str) -> Result<String, String> + Send + Sync + 'static,
    N: Fn(&AppHandle, &str, &str, &str) -> Result<(), String> + Send + Sync + 'static,
{
    let execute = Arc::new(execute);
    let notify = Arc::new(notify);
    thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let now = now_ms();
            let due = match due_tasks(&app, now) {
                Ok(tasks) => tasks,
                Err(_) => Vec::new(),
            };
            for (id, title, prompt, send_notification) in due {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let result = execute(&app, &prompt);
                let (status, mut summary) = match result {
                    Ok(text) => ("success".to_string(), truncate_summary(text)),
                    Err(error) => ("failed".to_string(), truncate_summary(error)),
                };
                if send_notification {
                    if let Err(error) = notify(&app, &title, &status, &summary) {
                        let notification_note =
                            format!("\n\n推送通知未送达：{}", truncate_summary(error));
                        summary = truncate_summary(format!("{summary}{notification_note}"));
                    }
                }
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

fn due_tasks(app: &AppHandle, now: i64) -> Result<Vec<(String, String, String, bool)>, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "定时任务存储锁定失败".to_string())?;
    Ok(load_unlocked(app)?
        .into_iter()
        .filter(|task| task.enabled && task.next_run_at_ms.is_some_and(|at| at <= now))
        .map(|task| (task.id, task.title, task.prompt, task.notify))
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
    trim_history(&mut task.history);
    match task.recurrence {
        Recurrence::Once => {
            task.enabled = false;
            task.next_run_at_ms = None;
        }
        recurrence => {
            let interval = interval_ms(recurrence, task.interval_minutes);
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

fn trim_history(history: &mut Vec<ExecutionRecord>) {
    if history.len() > MAX_HISTORY {
        let excess = history.len() - MAX_HISTORY;
        history.drain(0..excess);
    }
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
        validate_recurrence(task.recurrence, task.interval_minutes)?;
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

fn validate_recurrence(
    recurrence: Recurrence,
    interval_minutes: Option<u32>,
) -> Result<(), String> {
    match (recurrence, interval_minutes) {
        (Recurrence::Interval, Some(minutes)) if (1..=MAX_INTERVAL_MINUTES).contains(&minutes) => {
            Ok(())
        }
        (Recurrence::Interval, _) => {
            Err(format!("自定义间隔必须为 1 到 {MAX_INTERVAL_MINUTES} 分钟"))
        }
        (_, Some(_)) => Err("只有自定义间隔任务可以设置间隔分钟数".to_string()),
        (_, None) => Ok(()),
    }
}

fn interval_ms(recurrence: Recurrence, interval_minutes: Option<u32>) -> i64 {
    match recurrence {
        Recurrence::Once => 0,
        Recurrence::Hourly => 3_600_000,
        Recurrence::Daily => 86_400_000,
        Recurrence::Weekly => 604_800_000,
        Recurrence::Interval => i64::from(interval_minutes.unwrap_or(1)) * 60_000,
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

fn validate_channel(channel: &PushChannelConfig) -> Result<(), String> {
    validate_id(&channel.id)?;
    if channel.name.trim().is_empty() || channel.name.len() > 100 {
        return Err("推送渠道名称不能为空且不得超过 100 字节".to_string());
    }
    for value in [
        channel.endpoint.as_deref(),
        channel.secret.as_deref(),
        channel.target.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if value.len() > MAX_CHANNEL_FIELD_BYTES {
            return Err("推送渠道字段超过 4 KiB 限制".to_string());
        }
    }
    match channel.kind {
        PushChannelKind::Telegram => {
            let token = required_field(channel.secret.as_deref(), "Telegram Bot Token")?;
            required_field(channel.target.as_deref(), "Telegram Chat ID")?;
            if !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-'))
            {
                return Err("Telegram Bot Token 格式无效".to_string());
            }
        }
        PushChannelKind::ServerChan | PushChannelKind::PushPlus => {
            required_field(channel.secret.as_deref(), "推送密钥")?;
        }
        PushChannelKind::DingTalk => {
            validate_endpoint(required_field(
                channel.endpoint.as_deref(),
                "钉钉机器人 Webhook",
            )?)?;
        }
        PushChannelKind::Feishu | PushChannelKind::QqWebhook | PushChannelKind::Webhook => {
            validate_endpoint(required_field(channel.endpoint.as_deref(), "Webhook 地址")?)?;
        }
        PushChannelKind::Bark => {
            validate_endpoint(required_field(
                channel.endpoint.as_deref(),
                "Bark 服务器地址",
            )?)?;
            required_field(channel.secret.as_deref(), "Bark 设备 Key")?;
        }
    }
    if channel.kind == PushChannelKind::DingTalk {
        if let Some(secret) = channel
            .secret
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            if secret.len() > MAX_CHANNEL_FIELD_BYTES {
                return Err("钉钉签名密钥超过 4 KiB 限制".to_string());
            }
        }
    }
    Ok(())
}

fn required_field<'a>(value: Option<&'a str>, label: &str) -> Result<&'a str, String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("请填写 {label}"))
}

fn validate_endpoint(value: &str) -> Result<(), String> {
    let url = Url::parse(value).map_err(|_| "推送地址不是有效 URL".to_string())?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("推送地址须为有效 HTTP(S) URL，且不能包含用户名、密码或片段".to_string());
    }
    Ok(())
}

pub fn send_task_notification(
    state: &PushChannelState,
    title: &str,
    status: &str,
    summary: &str,
) -> Result<(), String> {
    let channels = state
        .channels
        .lock()
        .map_err(|_| "推送渠道状态锁定失败".to_string())?
        .clone();
    if channels.is_empty() {
        return Err("尚未配置推送渠道".to_string());
    }
    let status_label = if status == "success" {
        "成功"
    } else {
        "失败"
    };
    let message = truncate_summary(format!("任务：{title}\n状态：{status_label}\n\n{summary}"));
    let mut failures = Vec::new();
    for channel in channels {
        if let Err(error) = send_to_channel(&channel, title, &message) {
            failures.push(format!("{}：{}", channel.name, error));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(truncate_summary(failures.join("；")))
    }
}

fn send_to_channel(channel: &PushChannelConfig, title: &str, message: &str) -> Result<(), String> {
    validate_channel(channel)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("无法初始化推送客户端：{error}"))?;
    let response = match channel.kind {
        PushChannelKind::Telegram => {
            let token = channel.secret.as_deref().unwrap_or_default();
            let url = format!("https://api.telegram.org/bot{token}/sendMessage");
            client
                .post(url)
                .json(&serde_json::json!({
                    "chat_id": channel.target.as_deref().unwrap_or_default(),
                    "text": message
                }))
                .send()
        }
        PushChannelKind::ServerChan => {
            let mut url =
                Url::parse("https://sctapi.ftqq.com/").map_err(|error| error.to_string())?;
            {
                let mut segments = url
                    .path_segments_mut()
                    .map_err(|_| "无法构建 Server 酱地址".to_string())?;
                segments.pop_if_empty();
                segments.push(channel.secret.as_deref().unwrap_or_default());
                segments.push("send");
            }
            client
                .post(url)
                .form(&[("title", title), ("desp", message)])
                .send()
        }
        PushChannelKind::PushPlus => client
            .post("https://www.pushplus.plus/send")
            .json(&serde_json::json!({
                "token": channel.secret.as_deref().unwrap_or_default(),
                "title": title,
                "content": message
            }))
            .send(),
        PushChannelKind::DingTalk => {
            let mut url = Url::parse(channel.endpoint.as_deref().unwrap_or_default())
                .map_err(|_| "钉钉 Webhook 地址无效".to_string())?;
            if let Some(secret) = channel
                .secret
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                let timestamp = now_ms().to_string();
                let signed = format!("{timestamp}\n{secret}");
                let sign = base64_encode(&hmac_sha256(secret.as_bytes(), signed.as_bytes()));
                url.query_pairs_mut()
                    .append_pair("timestamp", &timestamp)
                    .append_pair("sign", &sign);
            }
            client
                .post(url)
                .json(&serde_json::json!({
                    "msgtype": "text",
                    "text": { "content": message }
                }))
                .send()
        }
        PushChannelKind::Feishu => client
            .post(channel.endpoint.as_deref().unwrap_or_default())
            .json(&serde_json::json!({
                "msg_type": "text",
                "content": { "text": message }
            }))
            .send(),
        PushChannelKind::Bark => {
            let mut url = Url::parse(channel.endpoint.as_deref().unwrap_or_default())
                .map_err(|_| "Bark 服务器地址无效".to_string())?;
            {
                let mut segments = url
                    .path_segments_mut()
                    .map_err(|_| "无法构建 Bark 地址".to_string())?;
                segments.pop_if_empty();
                segments.push(channel.secret.as_deref().unwrap_or_default());
            }
            client
                .post(url)
                .json(&serde_json::json!({ "title": title, "body": message }))
                .send()
        }
        PushChannelKind::QqWebhook | PushChannelKind::Webhook => client
            .post(channel.endpoint.as_deref().unwrap_or_default())
            .json(&serde_json::json!({ "title": title, "content": message }))
            .send(),
    }
    .map_err(|_| "推送请求失败；检查网络、服务地址或渠道凭据".to_string())?;
    let status_code = response.status();
    if !status_code.is_success() {
        return Err(format!("推送服务返回 HTTP {}", status_code.as_u16()));
    }
    let bytes = read_response_limited(response)?;
    validate_provider_response(channel.kind, &bytes)
}

fn read_response_limited(response: reqwest::blocking::Response) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PUSH_RESPONSE_BYTES)
    {
        return Err("推送服务响应超过 64 KiB 安全上限".to_string());
    }
    let mut limited = response.take(MAX_PUSH_RESPONSE_BYTES + 1);
    let mut bytes = Vec::new();
    limited
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取推送响应失败：{error}"))?;
    if bytes.len() as u64 > MAX_PUSH_RESPONSE_BYTES {
        return Err("推送服务响应超过 64 KiB 安全上限".to_string());
    }
    Ok(bytes)
}

fn validate_provider_response(kind: PushChannelKind, body: &[u8]) -> Result<(), String> {
    if matches!(kind, PushChannelKind::Webhook | PushChannelKind::QqWebhook) {
        return Ok(());
    }
    let payload: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| "推送服务返回了无法识别的响应".to_string())?;
    let accepted = match kind {
        PushChannelKind::Telegram => {
            payload.get("ok").and_then(serde_json::Value::as_bool) == Some(true)
        }
        PushChannelKind::ServerChan => payload.get("code").is_some_and(is_zero),
        PushChannelKind::PushPlus => payload
            .get("code")
            .is_some_and(|code| code == "200" || code == 200),
        PushChannelKind::DingTalk => payload.get("errcode").is_some_and(is_zero),
        PushChannelKind::Feishu => payload.get("code").is_some_and(is_zero),
        PushChannelKind::Bark => payload
            .get("code")
            .is_some_and(|code| code == 200 || code == "200"),
        PushChannelKind::QqWebhook | PushChannelKind::Webhook => true,
    };
    if accepted {
        Ok(())
    } else {
        let code = payload
            .get("code")
            .or_else(|| payload.get("errcode"))
            .map(|value| value.to_string())
            .unwrap_or_else(|| "未知错误".to_string());
        Err(format!("推送服务报告失败（代码 {code}）"))
    }
}

fn is_zero(value: &serde_json::Value) -> bool {
    value == 0 || value == "0"
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut key_block = [0u8; 64];
    if key.len() > key_block.len() {
        let digest = Sha256::digest(key);
        key_block[..digest.len()].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; 64];
    let mut outer_pad = [0x5cu8; 64];
    for index in 0..64 {
        inner_pad[index] ^= key_block[index];
        outer_pad[index] ^= key_block[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        output.push(TABLE[(first >> 2) as usize] as char);
        output.push(TABLE[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    #[test]
    fn interval_ms_supports_hour_day_week_and_custom_periods() {
        assert_eq!(interval_ms(Recurrence::Hourly, None), 3_600_000);
        assert_eq!(interval_ms(Recurrence::Daily, None), 86_400_000);
        assert_eq!(interval_ms(Recurrence::Weekly, None), 604_800_000);
        assert_eq!(interval_ms(Recurrence::Interval, Some(30)), 1_800_000);
    }

    #[test]
    fn interval_validation_requires_bounded_custom_period_only() {
        assert!(validate_recurrence(Recurrence::Interval, Some(1)).is_ok());
        assert!(validate_recurrence(Recurrence::Interval, Some(MAX_INTERVAL_MINUTES)).is_ok());
        assert!(validate_recurrence(Recurrence::Interval, Some(0)).is_err());
        assert!(validate_recurrence(Recurrence::Interval, Some(MAX_INTERVAL_MINUTES + 1)).is_err());
        assert!(validate_recurrence(Recurrence::Daily, Some(10)).is_err());
    }

    #[test]
    fn old_task_json_loads_with_notifications_disabled() {
        let old = r#"{
            "id":"old-1","title":"旧任务","prompt":"检查","scheduledAtMs":1700000000000,
            "recurrence":"daily","enabled":true,"nextRunAtMs":1700000000000,
            "lastRunAtMs":null,"lastStatus":null,"lastSummary":null,"history":[]
        }"#;
        let task: ScheduledTask = serde_json::from_str(old).expect("旧版任务应兼容反序列化");
        assert_eq!(task.recurrence, Recurrence::Daily);
        assert_eq!(task.interval_minutes, None);
        assert!(!task.notify);
    }

    #[test]
    fn custom_interval_and_notification_fields_round_trip_as_camel_case() {
        let task = ScheduledTask {
            id: "interval-1".to_string(),
            title: "间隔任务".to_string(),
            prompt: "检查状态".to_string(),
            scheduled_at_ms: 1_700_000_000_000,
            recurrence: Recurrence::Interval,
            interval_minutes: Some(45),
            notify: true,
            enabled: true,
            next_run_at_ms: Some(1_700_000_000_000),
            last_run_at_ms: None,
            last_status: None,
            last_summary: None,
            history: Vec::new(),
        };
        let value = serde_json::to_value(&task).expect("任务可序列化");
        assert_eq!(value["recurrence"], "interval");
        assert_eq!(value["intervalMinutes"], 45);
        assert_eq!(value["notify"], true);
        let loaded: ScheduledTask = serde_json::from_value(value).expect("自定义间隔任务可还原");
        assert_eq!(loaded.interval_minutes, Some(45));
        assert!(loaded.notify);
    }

    #[test]
    fn notification_credentials_are_not_in_ui_summaries() {
        let state = PushChannelState::default();
        let channel = PushChannelConfig {
            id: "test-telegram".to_string(),
            name: "测试 Telegram".to_string(),
            kind: PushChannelKind::Telegram,
            endpoint: None,
            secret: Some("12345:never-persist-this".to_string()),
            target: Some("chat".to_string()),
        };
        let summaries = state.save_channel(channel).expect("内存渠道配置有效");
        let json = serde_json::to_string(&summaries[0]).expect("摘要可序列化");
        assert!(!json.contains("never-persist-this"));
        assert!(json.contains("test-telegram"));
    }

    #[test]
    fn webhook_uses_only_a_loopback_mock_server_in_tests() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定临时回环端口");
        let address = listener.local_addr().expect("获取端口");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("接收回环请求");
            let mut request = Vec::new();
            let mut buffer = [0u8; 2048];
            loop {
                let count = stream.read(&mut buffer).expect("读取本地测试请求");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                let header_end = request.windows(4).position(|part| part == b"\r\n\r\n");
                if let Some(header_end) = header_end {
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
            }
            let body_start = request
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .expect("HTTP 头结束")
                .saturating_add(4);
            let body: serde_json::Value =
                serde_json::from_slice(&request[body_start..]).expect("Webhook JSON 请求体");
            assert_eq!(body["title"], "离线测试");
            assert_eq!(body["content"], "模拟通知正文");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .expect("返回本地测试响应");
        });
        let channel = PushChannelConfig {
            id: "local-webhook".to_string(),
            name: "仅本地 Mock".to_string(),
            kind: PushChannelKind::Webhook,
            endpoint: Some(format!("http://{address}/hook")),
            secret: None,
            target: None,
        };
        send_to_channel(&channel, "离线测试", "模拟通知正文").expect("本地 Mock 推送成功");
        server.join().expect("回环 Mock 服务结束");
    }

    #[test]
    fn provider_success_codes_are_checked() {
        assert!(validate_provider_response(PushChannelKind::Telegram, br#"{"ok":true}"#).is_ok());
        assert!(validate_provider_response(PushChannelKind::DingTalk, br#"{"errcode":0}"#).is_ok());
        assert!(
            validate_provider_response(PushChannelKind::PushPlus, br#"{"code":"200"}"#).is_ok()
        );
        assert!(validate_provider_response(PushChannelKind::Feishu, br#"{"code":19021}"#).is_err());
    }

    #[test]
    fn hmac_and_base64_primitives_match_standard_vector() {
        let digest = hmac_sha256(b"key", b"The quick brown fox jumps over the lazy dog");
        assert_eq!(
            base64_encode(&digest),
            "97yD9DBThCSxMpjmqm+xQ+9NWaFJRhdZl0edvC0aPNg="
        );
    }

    #[test]
    fn summary_is_bounded() {
        assert_eq!(truncate_summary("x".repeat(3_000)).len(), 2_000);
    }
}
