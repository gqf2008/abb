//! MCP event subscription server (`agent-bridge mcp-events`).
//!
//! The server is intentionally a small, synchronous stdio MCP endpoint. It is
//! spawned by ACP agents as a child process, so stdout is protocol-only and all
//! diagnostics go to stderr.
//!
//! v1 collects two event sources:
//! - walgit collaboration entries under `refs/collab/inbox/*`
//! - terminal ABB task states from `tasks/<bot>/tasks-state.json`
//!
//! Events are append-only in `events.ndjson`; subscriptions and cursors live in
//! `subs.json`. Cross-process file locking keeps duplicate collection safe when
//! multiple agent sessions run at once.

use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "abb-events";
const EVENTS_FILE: &str = "events.ndjson";
const SUBS_FILE: &str = "subs.json";
const LOCK_FILE: &str = "events.lock";
const SEEN_FILE: &str = "events.seen";
const DEFAULT_SUBSCRIPTION_ID: &str = "default";
const DEFAULT_WAIT_SECS: f64 = 30.0;
const MAX_WAIT_SECS: f64 = 300.0;
const DEFAULT_LIST_LIMIT: u64 = 20;
const MAX_LIST_LIMIT: u64 = 100;
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const WALGIT_FETCH_INTERVAL: Duration = Duration::from_secs(2);
/// 单文件上限；超过后轮转 `.1`/`.2`，旧段只用于审计，不参与 `events_list` 输出。
const EVENTS_MAX_BYTES: u64 = 5 * 1024 * 1024;
const EVENTS_ROTATE_KEEP: usize = 2;
const MAX_STRING_FIELD_CHARS: usize = 512;
const MAX_EVENT_TEXT_CHARS: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Event {
    pub id: String,
    pub ts: u64,
    pub kind: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub summary: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SubscriptionsFile {
    #[serde(default = "schema_version")]
    version: u32,
    #[serde(default)]
    subscriptions: BTreeMap<String, Subscription>,
}

fn schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Subscription {
    id: String,
    #[serde(default)]
    kinds: Vec<String>,
    #[serde(default)]
    thread: Option<String>,
    #[serde(default)]
    cursor: String,
    created_at: u64,
}

#[derive(Debug, Clone, PartialEq)]
enum WaitOutcome {
    Event(Event),
    Timeout,
}

#[derive(Debug)]
enum ToolCallError {
    Invalid(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ToolCallError {
    fn from(value: anyhow::Error) -> Self {
        Self::Internal(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    Subscribe,
    Wait,
    List,
    Unsubscribe,
}

impl Tool {
    fn all() -> [Self; 4] {
        [Self::Subscribe, Self::Wait, Self::List, Self::Unsubscribe]
    }

    fn name(self) -> &'static str {
        match self {
            Self::Subscribe => "events_subscribe",
            Self::Wait => "events_wait",
            Self::List => "events_list",
            Self::Unsubscribe => "events_unsubscribe",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::all().into_iter().find(|tool| tool.name() == name)
    }

    fn definition(self) -> Value {
        let kind_description = "可选事件类型过滤。walgit: issue, patch, status_needs_review, \
status_blocked, status_needs_human, review_needs_changes；任务: task_succeeded, task_failed, task_cancelled。";
        match self {
            Self::Subscribe => json!({
                "name": self.name(),
                "description": "创建持久订阅。游标从当前事件尾部开始，只返回之后的新事件。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "kinds": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": kind_description
                        },
                        "thread": {
                            "type": "string",
                            "description": "只接收指定 walgit thread id 的事件。"
                        }
                    },
                    "additionalProperties": false
                }
            }),
            Self::Wait => json!({
                "name": self.name(),
                "description": "长轮询等待下一条订阅事件；有事件立即返回，超时返回 {timeout:true}。subscription_id 省略时使用 default 订阅。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "subscription_id": {
                            "type": "string",
                            "description": "events_subscribe 返回的订阅 id；省略时使用 default。"
                        },
                        "timeout_secs": {
                            "type": "number",
                            "minimum": 0,
                            "maximum": 300,
                            "default": 30,
                            "description": "最长等待秒数，上限 300。"
                        }
                    },
                    "additionalProperties": false
                }
            }),
            Self::List => json!({
                "name": self.name(),
                "description": "列出最近事件，最新在前。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "limit": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 100,
                            "default": 20,
                            "description": "返回上限，默认 20，最大 100。"
                        }
                    },
                    "additionalProperties": false
                }
            }),
            Self::Unsubscribe => json!({
                "name": self.name(),
                "description": "删除订阅；不存在返回 {removed:false}。",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "subscription_id": {
                            "type": "string",
                            "description": "要删除的订阅 id。"
                        }
                    },
                    "required": ["subscription_id"],
                    "additionalProperties": false
                }
            }),
        }
    }
}

fn tool_definitions() -> Vec<Value> {
    Tool::all().into_iter().map(Tool::definition).collect()
}

/// Run the line-delimited JSON-RPC MCP server. Returns a process exit code.
pub fn run_stdio() -> i32 {
    let store = EventStore::from_env();
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return 0,
            Ok(_) => {}
            Err(error) => {
                eprintln!("[mcp-events] 读取 stdin 失败: {error}");
                return 1;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(response) = handle_message(&store, trimmed) else {
            continue;
        };
        let encoded = match serde_json::to_string(&response) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("[mcp-events] 序列化响应失败: {error}");
                continue;
            }
        };
        if let Err(error) = writer
            .write_all(encoded.as_bytes())
            .and_then(|_| writer.write_all(b"\n"))
            .and_then(|_| writer.flush())
        {
            eprintln!("[mcp-events] 写 stdout 失败: {error}");
            return 1;
        }
    }
}

fn handle_message(store: &EventStore, raw: &str) -> Option<Value> {
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("[mcp-events] JSON-RPC 解析失败: {error}");
            return Some(error_response(Value::Null, -32700, "Parse error"));
        }
    };
    let id = value.get("id").cloned();
    let notification = id.is_none();
    let response_id = id.unwrap_or(Value::Null);
    let method = match value.get("method").and_then(Value::as_str) {
        Some(method) if value.get("jsonrpc").and_then(Value::as_str) == Some("2.0") => method,
        _ => {
            if notification {
                return None;
            }
            return Some(error_response(response_id, -32600, "Invalid Request"));
        }
    };

    if notification {
        return None;
    }

    match method {
        "initialize" => Some(success_response(
            response_id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": SERVER_NAME,
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": "先调用 events_subscribe，再循环 events_wait 等待 walgit/任务事件。"
            }),
        )),
        "ping" => Some(success_response(response_id, json!({}))),
        "tools/list" => Some(success_response(
            response_id,
            json!({ "tools": tool_definitions() }),
        )),
        "tools/call" => {
            let params = value.get("params").cloned().unwrap_or_else(|| json!({}));
            let Some(params) = params.as_object() else {
                return Some(error_response(
                    response_id,
                    -32602,
                    "Invalid params: expected object",
                ));
            };
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error_response(
                    response_id,
                    -32602,
                    "Invalid params: missing tool name",
                ));
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match call_tool(store, name, &arguments) {
                Ok(result) => Some(success_response(
                    response_id,
                    json!({
                        "content": [{
                            "type": "text",
                            "text": serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string())
                        }],
                        "structuredContent": result
                    }),
                )),
                Err(ToolCallError::Invalid(message)) => {
                    Some(error_response(response_id, -32602, &message))
                }
                Err(ToolCallError::Internal(error)) => {
                    eprintln!("[mcp-events] tool {name} failed: {error:#}");
                    Some(error_response(response_id, -32603, "Internal error"))
                }
            }
        }
        _ => Some(error_response(
            response_id,
            -32601,
            &format!("Method not found: {method}"),
        )),
    }
}

fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn call_tool(
    store: &EventStore,
    name: &str,
    arguments: &Value,
) -> std::result::Result<Value, ToolCallError> {
    let tool = Tool::from_name(name)
        .ok_or_else(|| ToolCallError::Invalid(format!("Invalid params: unknown tool {name}")))?;
    let object = arguments.as_object().ok_or_else(|| {
        ToolCallError::Invalid("Invalid params: arguments must be an object".into())
    })?;
    match tool {
        Tool::Subscribe => {
            reject_unknown(object, &["kinds", "thread"])?;
            let kinds = parse_kinds(object.get("kinds"))?;
            let thread =
                parse_optional_string(object.get("thread"), "thread", MAX_STRING_FIELD_CHARS)?;
            let (subscription_id, cursor) = store.subscribe(kinds, thread)?;
            Ok(json!({
                "subscription_id": subscription_id,
                "cursor": cursor,
                "warnings": store.warnings(),
            }))
        }
        Tool::Wait => {
            reject_unknown(object, &["subscription_id", "timeout_secs"])?;
            let subscription_id = parse_optional_string(
                object.get("subscription_id"),
                "subscription_id",
                MAX_STRING_FIELD_CHARS,
            )?;
            let timeout = parse_timeout(object.get("timeout_secs"))?;
            let warnings = store.warnings();
            match store.wait(subscription_id.as_deref(), timeout)? {
                WaitOutcome::Event(event) => Ok(json!({ "event": event, "warnings": warnings })),
                WaitOutcome::Timeout => Ok(json!({ "timeout": true, "warnings": warnings })),
            }
        }
        Tool::List => {
            reject_unknown(object, &["limit"])?;
            let limit = parse_limit(object.get("limit"))?;
            let events = store.list(limit)?;
            Ok(json!({ "events": events, "warnings": store.warnings() }))
        }
        Tool::Unsubscribe => {
            reject_unknown(object, &["subscription_id"])?;
            let subscription_id = parse_required_string(
                object.get("subscription_id"),
                "subscription_id",
                MAX_STRING_FIELD_CHARS,
            )?;
            let removed = store.unsubscribe(&subscription_id)?;
            Ok(json!({ "removed": removed }))
        }
    }
}

fn reject_unknown(
    object: &Map<String, Value>,
    allowed: &[&str],
) -> std::result::Result<(), ToolCallError> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(ToolCallError::Invalid(format!(
            "Invalid params: unknown argument {key}"
        )));
    }
    Ok(())
}

fn parse_kinds(value: Option<&Value>) -> std::result::Result<Vec<String>, ToolCallError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let array = value
        .as_array()
        .ok_or_else(|| ToolCallError::Invalid("Invalid params: kinds must be an array".into()))?;
    if array.len() > 64 {
        return Err(ToolCallError::Invalid(
            "Invalid params: kinds has more than 64 entries".into(),
        ));
    }
    let mut kinds = Vec::with_capacity(array.len());
    for item in array {
        let kind = item
            .as_str()
            .ok_or_else(|| {
                ToolCallError::Invalid("Invalid params: kinds entries must be strings".into())
            })?
            .trim();
        if kind.is_empty() || kind.chars().count() > 64 {
            return Err(ToolCallError::Invalid(
                "Invalid params: kinds entries must be non-empty and <=64 chars".into(),
            ));
        }
        kinds.push(kind.to_string());
    }
    Ok(kinds)
}

fn parse_optional_string(
    value: Option<&Value>,
    field: &str,
    max_chars: usize,
) -> std::result::Result<Option<String>, ToolCallError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let text = value.as_str().ok_or_else(|| {
        ToolCallError::Invalid(format!("Invalid params: {field} must be a string"))
    })?;
    let text = text.trim();
    if text.is_empty() {
        return Err(ToolCallError::Invalid(format!(
            "Invalid params: {field} must not be empty"
        )));
    }
    if text.chars().count() > max_chars {
        return Err(ToolCallError::Invalid(format!(
            "Invalid params: {field} is too long"
        )));
    }
    Ok(Some(text.to_string()))
}

fn parse_required_string(
    value: Option<&Value>,
    field: &str,
    max_chars: usize,
) -> std::result::Result<String, ToolCallError> {
    parse_optional_string(value, field, max_chars)?
        .ok_or_else(|| ToolCallError::Invalid(format!("Invalid params: missing {field}")))
}

fn parse_timeout(value: Option<&Value>) -> std::result::Result<Duration, ToolCallError> {
    let Some(value) = value else {
        return Ok(Duration::from_secs_f64(DEFAULT_WAIT_SECS));
    };
    if value.is_null() {
        return Ok(Duration::from_secs_f64(DEFAULT_WAIT_SECS));
    }
    let seconds = value.as_f64().ok_or_else(|| {
        ToolCallError::Invalid("Invalid params: timeout_secs must be a number".into())
    })?;
    if !seconds.is_finite() || !(0.0..=MAX_WAIT_SECS).contains(&seconds) {
        return Err(ToolCallError::Invalid(
            "Invalid params: timeout_secs must be between 0 and 300".into(),
        ));
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn parse_limit(value: Option<&Value>) -> std::result::Result<usize, ToolCallError> {
    let Some(value) = value else {
        return Ok(DEFAULT_LIST_LIMIT as usize);
    };
    if value.is_null() {
        return Ok(DEFAULT_LIST_LIMIT as usize);
    }
    let limit = value
        .as_u64()
        .ok_or_else(|| ToolCallError::Invalid("Invalid params: limit must be an integer".into()))?;
    if limit > MAX_LIST_LIMIT {
        return Err(ToolCallError::Invalid(
            "Invalid params: limit must be between 0 and 100".into(),
        ));
    }
    Ok(limit as usize)
}

#[derive(Default)]
struct EventCache {
    offset: u64,
    events: Vec<Event>,
}

#[derive(Default)]
struct SeenCache {
    offset: u64,
    ids: BTreeSet<String>,
}

struct EventStore {
    root: PathBuf,
    collectors: EventCollectors,
    warnings: Vec<String>,
    last_refresh: Mutex<Option<Instant>>,
    event_cache: Mutex<EventCache>,
    seen_cache: Mutex<SeenCache>,
}

impl EventStore {
    fn from_env() -> Self {
        let root = crate::bridge_dir();
        let mut warnings = Vec::new();
        let env_repo = std::env::var_os("ABB_EVENTS_REPO")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let config_repo = crate::config::Config::events_walgit_repo();
        let cwd_repo = std::env::current_dir().ok();

        let repo = env_repo
            .as_deref()
            .and_then(|path| validate_walgit_repo(path, "ABB_EVENTS_REPO", &mut warnings))
            .or_else(|| {
                config_repo.as_deref().and_then(|path| {
                    validate_walgit_repo(path, "config.json events.walgit_repo", &mut warnings)
                })
            })
            .or_else(|| {
                cwd_repo
                    .as_deref()
                    .and_then(|path| validate_walgit_repo(path, "current directory", &mut warnings))
            });

        if repo.is_none() {
            warnings.push(
                "未配置事件仓库：请设置 config.json 的 events.walgit_repo 或环境变量 ABB_EVENTS_REPO；walgit 事件源当前为空"
                    .to_string(),
            );
        }
        for warning in &warnings {
            eprintln!("[mcp-events] {warning}");
        }
        let remote = std::env::var("ABB_EVENTS_WALGIT_REMOTE")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty() && value != "none");
        Self::new_with_warnings(root, repo, remote, warnings)
    }

    #[cfg(test)]
    fn new(root: PathBuf, repo: Option<PathBuf>, remote: Option<String>) -> Self {
        let warnings = repo
            .is_none()
            .then(|| "未配置事件仓库：walgit 事件源当前为空".to_string())
            .into_iter()
            .collect();
        Self::new_with_warnings(root, repo, remote, warnings)
    }

    fn new_with_warnings(
        root: PathBuf,
        repo: Option<PathBuf>,
        remote: Option<String>,
        warnings: Vec<String>,
    ) -> Self {
        let collectors = EventCollectors {
            tasks_root: root.join("tasks"),
            repo,
            remote,
            walgit_last_fetch: Mutex::new(None),
            walgit_seen_oids: Mutex::new(BTreeSet::new()),
        };
        Self {
            root,
            collectors,
            warnings,
            last_refresh: Mutex::new(None),
            event_cache: Mutex::new(EventCache::default()),
            seen_cache: Mutex::new(SeenCache::default()),
        }
    }

    fn warnings(&self) -> Vec<String> {
        self.warnings.clone()
    }

    fn events_path(&self) -> PathBuf {
        self.root.join(EVENTS_FILE)
    }

    fn seen_path(&self) -> PathBuf {
        self.root.join(SEEN_FILE)
    }

    fn subs_path(&self) -> PathBuf {
        self.root.join(SUBS_FILE)
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join(LOCK_FILE)
    }

    fn with_lock<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("创建事件目录失败：{}", self.root.display()))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.lock_path())
            .context("打开事件锁文件失败")?;
        FileExt::lock_exclusive(&file).context("锁定事件文件失败")?;
        let result = operation();
        let unlock_result = FileExt::unlock(&file).context("解锁事件文件失败");
        match (result, unlock_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    fn sync_event_cache_unlocked(&self) -> Result<()> {
        let path = self.events_path();
        let mut cache = self
            .event_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let length = match fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("读取事件文件元数据失败：{}", path.display()))
            }
        };
        if length < cache.offset {
            cache.offset = 0;
            cache.events.clear();
        }
        if length == cache.offset {
            return Ok(());
        }
        let mut file =
            File::open(&path).with_context(|| format!("打开事件文件失败：{}", path.display()))?;
        file.seek(SeekFrom::Start(cache.offset))?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            let line_start = cache.offset;
            line.clear();
            let read = reader.read_line(&mut line)?;
            if read == 0 {
                break;
            }
            cache.offset = line_start + read as u64;
            if !line.ends_with('\n') {
                // 只消费完整行；下次从这一行的起点重新读。
                cache.offset = line_start;
                break;
            }
            let text = line.trim();
            if text.is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(text) {
                Ok(event) => cache.events.push(event),
                Err(error) => {
                    eprintln!("[mcp-events] 跳过损坏事件行 offset={}：{error}", line_start)
                }
            }
        }
        Ok(())
    }

    fn read_events_unlocked(&self) -> Result<Vec<Event>> {
        self.sync_event_cache_unlocked()?;
        Ok(self
            .event_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .events
            .clone())
    }

    fn load_subscriptions_unlocked(&self) -> Result<SubscriptionsFile> {
        let path = self.subs_path();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SubscriptionsFile {
                    version: schema_version(),
                    subscriptions: BTreeMap::new(),
                })
            }
            Err(error) => {
                return Err(error).with_context(|| format!("读取订阅文件失败：{}", path.display()))
            }
        };
        let data: SubscriptionsFile = serde_json::from_str(&text)
            .with_context(|| format!("解析订阅文件失败：{}", path.display()))?;
        if data.version > schema_version() {
            bail!(
                "订阅文件 schema 版本 {} 高于支持的 {}",
                data.version,
                schema_version()
            );
        }
        Ok(data)
    }

    fn save_subscriptions_unlocked(&self, data: &SubscriptionsFile) -> Result<()> {
        let text = serde_json::to_string_pretty(data)?;
        crate::atomic_write_text(&self.subs_path(), &text)
            .with_context(|| format!("写入订阅文件失败：{}", self.subs_path().display()))
    }

    fn sync_seen_cache_unlocked(&self, seen: &mut SeenCache) -> Result<()> {
        let path = self.seen_path();
        let length = match fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("读取去重索引元数据失败：{}", path.display()))
            }
        };
        if length < seen.offset {
            seen.offset = 0;
            seen.ids.clear();
        }
        if length == seen.offset {
            return Ok(());
        }
        let mut file =
            File::open(&path).with_context(|| format!("打开去重索引失败：{}", path.display()))?;
        file.seek(SeekFrom::Start(seen.offset))?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            let line_start = seen.offset;
            line.clear();
            let read = reader.read_line(&mut line)?;
            if read == 0 {
                break;
            }
            seen.offset = line_start + read as u64;
            if !line.ends_with('\n') {
                seen.offset = line_start;
                break;
            }
            let id = line.trim();
            if !id.is_empty() {
                seen.ids.insert(id.to_string());
            }
        }
        Ok(())
    }

    fn append_seen_id_unlocked(&self, id: &str) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .append(true)
            .open(self.seen_path())
            .context("打开去重索引追加文件失败")?;
        writeln!(file, "{id}")?;
        file.sync_data()?;
        Ok(())
    }

    fn rotate_events_unlocked(&self) -> Result<()> {
        let path = self.events_path();
        for index in (1..=EVENTS_ROTATE_KEEP).rev() {
            let target = rotated_path(&path, index);
            let source = if index == 1 {
                path.clone()
            } else {
                rotated_path(&path, index - 1)
            };
            if !source.exists() {
                continue;
            }
            if target.exists() {
                fs::remove_file(&target)
                    .with_context(|| format!("清理旧事件段失败：{}", target.display()))?;
            }
            fs::rename(&source, &target).with_context(|| {
                format!(
                    "轮转事件文件失败：{} -> {}",
                    source.display(),
                    target.display()
                )
            })?;
        }
        Ok(())
    }

    fn repair_event_tail_unlocked(&self) -> Result<()> {
        let path = self.events_path();
        let length = match fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("读取事件文件元数据失败：{}", path.display()))
            }
        };
        if length == 0 {
            return Ok(());
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("打开事件文件做边界修复失败：{}", path.display()))?;
        let mut last = [0u8; 1];
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut last)?;
        if last[0] == b'\n' {
            return Ok(());
        }

        let mut cursor = length;
        let mut buffer = [0u8; 4096];
        let mut last_newline = None;
        while cursor > 0 {
            let take = cursor.min(buffer.len() as u64) as usize;
            cursor -= take as u64;
            file.seek(SeekFrom::Start(cursor))?;
            file.read_exact(&mut buffer[..take])?;
            if let Some(index) = buffer[..take].iter().rposition(|byte| *byte == b'\n') {
                last_newline = Some(cursor + index as u64);
                break;
            }
        }
        let tail_start = last_newline.map(|index| index + 1).unwrap_or(0);
        let tail_len = (length - tail_start) as usize;
        let mut tail = vec![0u8; tail_len];
        file.seek(SeekFrom::Start(tail_start))?;
        file.read_exact(&mut tail)?;
        if serde_json::from_slice::<Event>(&tail).is_ok() {
            file.seek(SeekFrom::End(0))?;
            file.write_all(b"\n")?;
            file.sync_data()?;
        } else {
            file.set_len(tail_start)?;
            file.sync_data()?;
        }
        Ok(())
    }

    fn append_event_unlocked(&self, event: &Event) -> Result<()> {
        self.repair_event_tail_unlocked()?;
        let mut line = serde_json::to_vec(event)?;
        line.push(b'\n');
        let path = self.events_path();
        let length = fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if length > 0 && length.saturating_add(line.len() as u64) > EVENTS_MAX_BYTES {
            self.rotate_events_unlocked()?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .append(true)
            .open(&path)
            .context("打开事件追加文件失败")?;
        file.write_all(&line)?;
        Ok(())
    }

    fn known_event_ids(&self) -> Result<BTreeSet<String>> {
        self.with_lock(|| {
            let mut seen = self
                .seen_cache
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            self.sync_seen_cache_unlocked(&mut seen)?;
            Ok(seen.ids.clone())
        })
    }

    fn refresh(&self) -> Result<usize> {
        let known_ids = self.known_event_ids()?;
        let mut pending = self.collectors.collect_all(&known_ids);
        pending.sort_by(|left, right| left.ts.cmp(&right.ts).then_with(|| left.id.cmp(&right.id)));
        self.with_lock(|| {
            let current = self.read_events_unlocked()?;
            let mut seen = self
                .seen_cache
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            self.sync_seen_cache_unlocked(&mut seen)?;
            for event in current {
                if seen.ids.insert(event.id.clone()) {
                    self.append_seen_id_unlocked(&event.id)?;
                }
            }
            let mut appended = 0usize;
            for event in pending {
                if seen.ids.contains(&event.id) {
                    continue;
                }
                self.append_event_unlocked(&event)?;
                self.append_seen_id_unlocked(&event.id)?;
                seen.ids.insert(event.id);
                appended += 1;
            }
            Ok(appended)
        })
    }

    fn refresh_if_due(&self, force: bool) -> Result<usize> {
        {
            let mut last_refresh = self
                .last_refresh
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !force
                && last_refresh
                    .is_some_and(|last_refresh| last_refresh.elapsed() < REFRESH_INTERVAL)
            {
                return Ok(0);
            }
            *last_refresh = Some(Instant::now());
        }
        self.refresh()
    }

    fn subscribe(&self, kinds: Vec<String>, thread: Option<String>) -> Result<(String, String)> {
        self.refresh_if_due(true)?;
        self.with_lock(|| {
            let mut data = self.load_subscriptions_unlocked()?;
            let cursor = self
                .read_events_unlocked()?
                .last()
                .map(|event| event.id.clone())
                .unwrap_or_default();
            let subscription_id = format!("sub_{}", uuid::Uuid::new_v4().simple());
            data.subscriptions.insert(
                subscription_id.clone(),
                Subscription {
                    id: subscription_id.clone(),
                    kinds,
                    thread,
                    cursor: cursor.clone(),
                    created_at: now_secs(),
                },
            );
            self.save_subscriptions_unlocked(&data)?;
            Ok((subscription_id, cursor))
        })
    }

    fn ensure_wait_subscription(&self, requested: Option<&str>) -> Result<Subscription> {
        self.refresh_if_due(true)?;
        self.with_lock(|| {
            let mut data = self.load_subscriptions_unlocked()?;
            if let Some(id) = requested {
                return data
                    .subscriptions
                    .get(id)
                    .cloned()
                    .with_context(|| format!("订阅不存在：{id}"));
            }
            if let Some(subscription) = data.subscriptions.get(DEFAULT_SUBSCRIPTION_ID).cloned() {
                return Ok(subscription);
            }
            let cursor = self
                .read_events_unlocked()?
                .last()
                .map(|event| event.id.clone())
                .unwrap_or_default();
            let subscription = Subscription {
                id: DEFAULT_SUBSCRIPTION_ID.to_string(),
                kinds: Vec::new(),
                thread: None,
                cursor,
                created_at: now_secs(),
            };
            data.subscriptions
                .insert(DEFAULT_SUBSCRIPTION_ID.to_string(), subscription.clone());
            self.save_subscriptions_unlocked(&data)?;
            Ok(subscription)
        })
    }

    fn next_event(&self, subscription_id: &str) -> Result<Option<Event>> {
        self.with_lock(|| {
            let mut data = self.load_subscriptions_unlocked()?;
            let mut subscription = data
                .subscriptions
                .get(subscription_id)
                .cloned()
                .with_context(|| format!("订阅不存在：{subscription_id}"))?;
            let events = self.read_events_unlocked()?;
            let start = if subscription.cursor.is_empty() {
                0
            } else {
                events
                    .iter()
                    .position(|event| event.id == subscription.cursor)
                    .map(|index| index + 1)
                    .unwrap_or(0)
            };
            let mut latest = subscription.cursor.clone();
            for event in events.iter().skip(start) {
                latest.clone_from(&event.id);
                if event_matches(&subscription, event) {
                    subscription.cursor.clone_from(&event.id);
                    data.subscriptions
                        .insert(subscription_id.to_string(), subscription.clone());
                    self.save_subscriptions_unlocked(&data)?;
                    return Ok(Some(event.clone()));
                }
            }
            if latest != subscription.cursor {
                subscription.cursor = latest;
                data.subscriptions
                    .insert(subscription_id.to_string(), subscription);
                self.save_subscriptions_unlocked(&data)?;
            }
            Ok(None)
        })
    }

    fn wait(&self, subscription_id: Option<&str>, timeout: Duration) -> Result<WaitOutcome> {
        let subscription = self.ensure_wait_subscription(subscription_id)?;
        let deadline = Instant::now() + timeout;
        loop {
            self.refresh_if_due(false)?;
            if let Some(event) = self.next_event(&subscription.id)? {
                return Ok(WaitOutcome::Event(event));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(WaitOutcome::Timeout);
            }
            std::thread::sleep((deadline - now).min(POLL_INTERVAL));
        }
    }

    fn list(&self, limit: usize) -> Result<Vec<Event>> {
        self.refresh_if_due(true)?;
        self.with_lock(|| {
            let mut events = self.read_events_unlocked()?;
            events
                .sort_by(|left, right| right.ts.cmp(&left.ts).then_with(|| right.id.cmp(&left.id)));
            events.truncate(limit);
            Ok(events)
        })
    }

    fn unsubscribe(&self, subscription_id: &str) -> Result<bool> {
        self.with_lock(|| {
            let mut data = self.load_subscriptions_unlocked()?;
            let removed = data.subscriptions.remove(subscription_id).is_some();
            if removed {
                self.save_subscriptions_unlocked(&data)?;
            }
            Ok(removed)
        })
    }

    #[cfg(test)]
    fn append_for_test(&self, event: Event) -> Result<()> {
        self.with_lock(|| {
            self.append_event_unlocked(&event)?;
            self.append_seen_id_unlocked(&event.id)?;
            Ok(())
        })
    }
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    PathBuf::from(format!("{}.{index}", path.display()))
}

fn validate_walgit_repo(path: &Path, source: &str, warnings: &mut Vec<String>) -> Option<PathBuf> {
    if !path.is_absolute() {
        warnings.push(format!("{source} 不是绝对路径：{}", path.display()));
        return None;
    }
    if !path.exists() {
        warnings.push(format!("{source} 路径不存在：{}", path.display()));
        return None;
    }
    let git_dir = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--git-dir"])
        .output();
    if !git_dir.as_ref().is_ok_and(|output| output.status.success()) {
        warnings.push(format!("{source} 不是 git 仓库：{}", path.display()));
        return None;
    }
    let origin = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["remote", "get-url", "origin"])
        .output();
    if !origin.as_ref().is_ok_and(|output| output.status.success()) {
        warnings.push(format!("{source} 没有 origin remote：{}", path.display()));
        return None;
    }
    Some(path.to_path_buf())
}

fn event_matches(subscription: &Subscription, event: &Event) -> bool {
    if !subscription.kinds.is_empty() && !subscription.kinds.iter().any(|kind| kind == &event.kind)
    {
        return false;
    }
    if let Some(thread) = subscription.thread.as_deref() {
        if event.thread.as_deref() != Some(thread) {
            return false;
        }
    }
    true
}

struct EventCollectors {
    tasks_root: PathBuf,
    repo: Option<PathBuf>,
    remote: Option<String>,
    walgit_last_fetch: Mutex<Option<Instant>>,
    walgit_seen_oids: Mutex<BTreeSet<String>>,
}

impl EventCollectors {
    fn collect_all(&self, known_event_ids: &BTreeSet<String>) -> Vec<Event> {
        let mut events = self.collect_tasks();
        events.extend(self.collect_walgit(known_event_ids));
        events
    }

    fn fetch_walgit_if_due(&self) {
        let (Some(repo), Some(remote)) = (self.repo.as_deref(), self.remote.as_deref()) else {
            return;
        };
        {
            let last_fetch = self
                .walgit_last_fetch
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if last_fetch.is_some_and(|last_fetch| last_fetch.elapsed() < WALGIT_FETCH_INTERVAL) {
                return;
            }
        }
        {
            let mut last_fetch = self
                .walgit_last_fetch
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *last_fetch = Some(Instant::now());
        }
        let refspec = "+refs/collab/inbox/*:refs/collab/inbox/*";
        match Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["fetch", "--quiet", "--no-tags", remote, refspec])
            .output()
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => eprintln!(
                "[mcp-events] walgit fetch 失败（继续用本地 refs）：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(error) => eprintln!("[mcp-events] 无法执行 git fetch：{error}"),
        }
    }

    fn collect_tasks(&self) -> Vec<Event> {
        let entries = match fs::read_dir(&self.tasks_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                eprintln!(
                    "[mcp-events] 读取任务目录失败（{}）：{error}",
                    self.tasks_root.display()
                );
                return Vec::new();
            }
        };
        let mut bot_dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        bot_dirs.sort();

        let mut events = Vec::new();
        for bot_dir in bot_dirs {
            let bot_key = bot_dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            let names = read_task_display_names(&bot_dir.join("tasks.json"));
            let states_path = bot_dir.join("tasks-state.json");
            let text = match fs::read_to_string(&states_path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    eprintln!(
                        "[mcp-events] 读取任务状态失败（{}）：{error}",
                        states_path.display()
                    );
                    continue;
                }
            };
            let states: Value = match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(error) => {
                    eprintln!(
                        "[mcp-events] 解析任务状态失败（{}）：{error}",
                        states_path.display()
                    );
                    continue;
                }
            };
            let Some(states) = states.as_object() else {
                eprintln!("[mcp-events] 任务状态不是对象（{}）", states_path.display());
                continue;
            };
            for (task_id, runtime) in states {
                let Some(state) = runtime.get("kind").and_then(Value::as_str) else {
                    continue;
                };
                let state = state.to_ascii_lowercase();
                let (kind, verb) = match state.as_str() {
                    "succeeded" => ("task_succeeded", "成功完成"),
                    "failed" => ("task_failed", "失败"),
                    "cancelled" => ("task_cancelled", "已取消"),
                    _ => continue,
                };
                let display_name = snippet(
                    &names
                        .get(task_id)
                        .filter(|name| !name.trim().is_empty())
                        .cloned()
                        .unwrap_or_else(|| task_id.chars().take(12).collect()),
                    MAX_EVENT_TEXT_CHARS,
                );
                let last_error_full = runtime
                    .get("last_error")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let last_error = snippet(&last_error_full, MAX_EVENT_TEXT_CHARS);
                let finished_at = runtime.get("finished_at").cloned().unwrap_or(Value::Null);
                // Dedup on the terminal transition identity, not incidental runtime
                // fields (pid/restarts): the same terminal state must not re-alert.
                let fingerprint = serde_json::to_string(&json!({
                    "bot_key": bot_key,
                    "task_id": task_id,
                    "state": state,
                    "last_error": last_error_full,
                    "finished_at": finished_at,
                    "last_exit_code": runtime.get("last_exit_code"),
                }))
                .unwrap_or_default();
                let id = deterministic_event_id("task", &[&fingerprint]);
                let ts = finished_at.as_u64().unwrap_or_else(now_secs);
                let raw_summary = if kind == "task_failed" && !last_error.is_empty() {
                    format!("任务「{display_name}」失败（{task_id}）：{last_error}")
                } else if !last_error.is_empty() {
                    format!("任务「{display_name}」{verb}（{task_id}）：{last_error}")
                } else {
                    format!("任务「{display_name}」{verb}（{task_id}）")
                };
                events.push(Event {
                    id,
                    ts,
                    kind: kind.to_string(),
                    source: "task".to_string(),
                    thread: None,
                    task_id: Some(task_id.clone()),
                    summary: snippet(&raw_summary, MAX_EVENT_TEXT_CHARS),
                    payload: json!({
                        "task_id": task_id,
                        "display_name": display_name,
                        "state": state,
                        "last_error": last_error,
                        "finished_at": finished_at,
                    }),
                });
            }
        }
        events
    }

    fn collect_walgit(&self, known_event_ids: &BTreeSet<String>) -> Vec<Event> {
        let Some(repo) = self.repo.as_deref() else {
            return Vec::new();
        };
        self.fetch_walgit_if_due();
        let refs = match git_output(
            repo,
            &[
                "for-each-ref",
                "--format=%(objectname)\t%(refname)",
                "refs/collab/inbox/",
            ],
        ) {
            Ok(output) => output,
            Err(error) => {
                eprintln!("[mcp-events] 读取 walgit collab refs 失败：{error:#}");
                return Vec::new();
            }
        };
        let mut unique: BTreeMap<String, String> = BTreeMap::new();
        for line in refs.lines() {
            let Some((object_id, ref_name)) = line.split_once('\t') else {
                continue;
            };
            unique
                .entry(object_id.trim().to_string())
                .or_insert_with(|| ref_name.trim().to_string());
        }

        let mut known_oids = self
            .walgit_seen_oids
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut new_oids = Vec::new();
        for object_id in unique.keys() {
            if known_oids.contains(object_id) {
                continue;
            }
            let event_id = deterministic_event_id("walgit", &[object_id]);
            if known_event_ids.contains(&event_id) {
                known_oids.insert(object_id.clone());
                continue;
            }
            new_oids.push(object_id.clone());
        }
        if new_oids.is_empty() {
            return Vec::new();
        }
        let contents = match git_cat_file_batch(repo, &new_oids) {
            Ok(contents) => contents,
            Err(error) => {
                eprintln!("[mcp-events] 批量读取 walgit 条目失败：{error:#}");
                return Vec::new();
            }
        };
        known_oids.extend(new_oids.iter().cloned());
        let mut events = Vec::new();
        for object_id in new_oids {
            let Some(ref_name) = unique.get(&object_id) else {
                continue;
            };
            let Some(raw) = contents.get(&object_id) else {
                eprintln!("[mcp-events] walgit batch 未返回条目 {object_id}");
                continue;
            };
            let entry: WalgitEntry = match serde_json::from_str(raw) {
                Ok(entry) => entry,
                Err(error) => {
                    eprintln!("[mcp-events] 跳过非 JSON walgit 条目 {object_id}: {error}");
                    continue;
                }
            };
            if let Some(event) = walgit_event(entry, &object_id, ref_name) {
                events.push(event);
            }
        }
        events
    }
}

#[derive(Debug, Deserialize)]
struct WalgitEntry {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    actor: String,
    #[serde(default)]
    ts: u64,
    #[serde(default)]
    refs: BTreeMap<String, String>,
    #[serde(default)]
    body: Value,
}

fn walgit_event(entry: WalgitEntry, object_id: &str, ref_name: &str) -> Option<Event> {
    let thread = entry.id.trim();
    if thread.is_empty() {
        return None;
    }
    let body = &entry.body;
    let title = body_string(body, "title");
    let note = body_string(body, "note");
    let work = body_string(body, "work");
    let status = body_string(body, "status");
    let decision = body_string(body, "decision");
    let branch = entry.refs.get("head").cloned().or_else(|| {
        let branch = body_string(body, "branch");
        (!branch.is_empty()).then_some(branch)
    });

    let (kind, summary, status_filter) = match entry.kind.as_str() {
        "issue" => (
            "issue",
            format!("新 issue「{}」（{}）", snippet(&title, 160), entry.actor),
            None,
        ),
        "patch" => (
            "patch",
            format!("新 patch「{}」（{}）", snippet(&title, 160), entry.actor),
            None,
        ),
        "status" if matches!(status.as_str(), "needs-review" | "blocked" | "needs-human") => {
            let event_kind = match status.as_str() {
                "needs-review" => "status_needs_review",
                "blocked" => "status_blocked",
                "needs-human" => "status_needs_human",
                _ => unreachable!(),
            };
            let detail = if work.is_empty() { &note } else { &work };
            (
                event_kind,
                format!(
                    "状态 {}：{}（{}）",
                    status,
                    snippet(detail, 180),
                    entry.actor
                ),
                Some(status.clone()),
            )
        }
        "review" if decision == "needs-changes" => (
            "review_needs_changes",
            format!(
                "评审 needs-changes：{}（{}）",
                snippet(&note, 220),
                entry.actor
            ),
            None,
        ),
        _ => return None,
    };

    let payload = json!({
        "entry_oid": object_id,
        "ref": ref_name,
        "actor": entry.actor,
        "raw_kind": entry.kind,
        "branch": branch,
        "status": status_filter,
        "decision": if decision.is_empty() { Value::Null } else { Value::String(decision) },
    });
    Some(Event {
        id: deterministic_event_id("walgit", &[object_id]),
        ts: if entry.ts == 0 { now_secs() } else { entry.ts },
        kind: kind.to_string(),
        source: "walgit".to_string(),
        thread: Some(thread.to_string()),
        task_id: None,
        summary: snippet(&summary, MAX_EVENT_TEXT_CHARS),
        payload,
    })
}

fn body_string(body: &Value, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn read_task_display_names(path: &Path) -> BTreeMap<String, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return BTreeMap::new(),
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            eprintln!(
                "[mcp-events] 解析任务定义失败（{}）：{error}",
                path.display()
            );
            return BTreeMap::new();
        }
    };
    let mut names = BTreeMap::new();
    if let Some(tasks) = value.as_array() {
        for task in tasks {
            let Some(id) = task.get("id").and_then(Value::as_str) else {
                continue;
            };
            let name = task
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if !name.is_empty() {
                names.insert(id.to_string(), name);
            }
        }
    }
    names
}

struct TempInputFile {
    path: PathBuf,
}

impl TempInputFile {
    fn new() -> Self {
        Self {
            path: std::env::temp_dir().join(format!(
                "abb-mcp-cat-file-{}.txt",
                uuid::Uuid::new_v4().simple()
            )),
        }
    }
}

impl Drop for TempInputFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn git_cat_file_batch(repo: &Path, object_ids: &[String]) -> Result<BTreeMap<String, String>> {
    // Never write a large oid list into a pipe before reading stdout: the
    // child can block writing its result while the parent blocks on stdin.
    // A temporary regular-file stdin gives git EOF without a pipe deadlock.
    let temp = TempInputFile::new();
    {
        let mut input = File::create(&temp.path).with_context(|| {
            format!(
                "创建 git cat-file 输入临时文件失败：{}",
                temp.path.display()
            )
        })?;
        for object_id in object_ids {
            writeln!(input, "{object_id}").context("写入 git cat-file batch 请求失败")?;
        }
        input.flush()?;
        input.sync_data()?;
    }
    let input = File::open(&temp.path).with_context(|| {
        format!(
            "打开 git cat-file 输入临时文件失败：{}",
            temp.path.display()
        )
    })?;
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "--batch"])
        .stdin(std::process::Stdio::from(input))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("执行 git cat-file --batch 失败")?;
    if !output.status.success() {
        bail!(
            "git cat-file --batch 退出码 {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let mut contents = BTreeMap::new();
    let bytes = output.stdout;
    let mut offset = 0usize;
    while offset < bytes.len() {
        let header_end = bytes[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| offset + index)
            .context("git cat-file batch 输出缺少 header 换行")?;
        let header = std::str::from_utf8(&bytes[offset..header_end])
            .context("git cat-file batch header 不是 UTF-8")?;
        offset = header_end + 1;
        let mut parts = header.split_whitespace();
        let object_id = parts.next().context("git cat-file batch header 缺少 oid")?;
        let object_type = parts.next().context("git cat-file batch header 缺少类型")?;
        if object_type == "missing" {
            continue;
        }
        let size: usize = parts
            .next()
            .context("git cat-file batch header 缺少 size")?
            .parse()
            .with_context(|| format!("git cat-file batch size 异常：{header}"))?;
        if offset + size > bytes.len() {
            bail!("git cat-file batch 内容越界：{header}");
        }
        let content = String::from_utf8_lossy(&bytes[offset..offset + size]).to_string();
        offset += size;
        if bytes.get(offset) == Some(&b'\n') {
            offset += 1;
        }
        contents.insert(object_id.to_string(), content);
    }
    Ok(contents)
}

fn git_output(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("执行 git {:?} 失败", args))?;
    if !output.status.success() {
        bail!(
            "git {:?} 退出码 {:?}: {}",
            args,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn deterministic_event_id(prefix: &str, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prefix.as_bytes());
    for part in parts {
        hasher.update([0]);
        hasher.update(part.as_bytes());
    }
    let digest = format!("{:x}", hasher.finalize());
    format!("evt_{prefix}_{}", &digest[..32])
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn snippet(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "abb-mcp-events-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    fn event(id: &str, ts: u64, kind: &str, thread: Option<&str>, task_id: Option<&str>) -> Event {
        Event {
            id: id.to_string(),
            ts,
            kind: kind.to_string(),
            source: if task_id.is_some() {
                "task".to_string()
            } else {
                "walgit".to_string()
            },
            thread: thread.map(str::to_string),
            task_id: task_id.map(str::to_string),
            summary: format!("event {id}"),
            payload: json!({ "test": true }),
        }
    }

    fn run_git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git command should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    fn write_walgit_entry(repo: &Path, json: &str) -> String {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["hash-object", "-w", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn git hash-object");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(json.as_bytes())
            .expect("write blob");
        let output = child.wait_with_output().expect("wait git hash-object");
        assert!(output.status.success());
        let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        run_git(
            repo,
            &[
                "update-ref",
                &format!("refs/collab/inbox/tester/{}", &oid[..12]),
                &oid,
            ],
        );
        oid
    }

    fn write_many_walgit_entries(repo: &Path, count: usize) -> Vec<String> {
        let blobs = repo.join("blobs");
        fs::create_dir_all(&blobs).unwrap();
        let mut paths = Vec::with_capacity(count);
        for index in 0..count {
            let relative = format!("blobs/entry-{index:05}.json");
            let body = json!({
                "version": 1,
                "kind": "issue",
                "id": format!("pipe-thread-{index:05}"),
                "actor": "pipe-test",
                "ts": index as u64 + 1,
                "body": { "title": format!("pipe fixture {index:05}") }
            });
            fs::write(repo.join(&relative), serde_json::to_vec(&body).unwrap()).unwrap();
            paths.push(relative);
        }

        let paths_input = TempInputFile::new();
        {
            let mut input = File::create(&paths_input.path).unwrap();
            for path in &paths {
                writeln!(input, "{path}").unwrap();
            }
        }
        let input = File::open(&paths_input.path).unwrap();
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["hash-object", "-w", "--stdin-paths"])
            .stdin(std::process::Stdio::from(input))
            .output()
            .unwrap();
        assert!(output.status.success(), "git hash-object failed");
        let oids: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(oids.len(), count);

        let refs_input = TempInputFile::new();
        {
            let mut input = File::create(&refs_input.path).unwrap();
            for (index, oid) in oids.iter().enumerate() {
                writeln!(
                    input,
                    "update refs/collab/inbox/pipe/entry-{index:05} {oid}"
                )
                .unwrap();
            }
        }
        let input = File::open(&refs_input.path).unwrap();
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["update-ref", "--stdin"])
            .stdin(std::process::Stdio::from(input))
            .output()
            .unwrap();
        assert!(output.status.success(), "git update-ref failed");
        oids
    }

    #[test]
    fn git_cat_file_batch_above_pipe_capacity_is_bounded() {
        let root = test_root("large-batch");
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        let oids = write_many_walgit_entries(&repo, 5_000);
        let input_bytes: usize = oids.iter().map(|oid| oid.len() + 1).sum();
        assert!(input_bytes > 64 * 1024, "fixture must exceed pipe capacity");

        let repo_for_worker = repo.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = git_cat_file_batch(&repo_for_worker, &oids);
            let _ = tx.send(result);
        });
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Ok(contents)) => assert_eq!(contents.len(), 5_000),
            Ok(Err(error)) => panic!("large batch failed: {error:#}"),
            Err(_) => {
                panic!("git cat-file batch did not finish within 20s (pipe deadlock regression)")
            }
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tool_definitions_are_single_source() {
        let definitions = tool_definitions();
        assert_eq!(definitions.len(), 4);
        let names: Vec<_> = definitions
            .iter()
            .filter_map(|definition| definition["name"].as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "events_subscribe",
                "events_wait",
                "events_list",
                "events_unsubscribe"
            ]
        );
        assert_eq!(
            definitions[0]["inputSchema"]["properties"]["kinds"]["type"],
            "array"
        );
        assert_eq!(
            definitions[3]["inputSchema"]["required"],
            json!(["subscription_id"])
        );
    }

    #[test]
    fn subscribe_wait_returns_immediately_for_matching_event() {
        let root = test_root("wait-match");
        let store = EventStore::new(root.clone(), None, None);
        let (subscription_id, cursor) = store.subscribe(Vec::new(), None).unwrap();
        assert!(cursor.is_empty());
        store
            .append_for_test(event("evt_1", 10, "task_failed", None, Some("tk_1")))
            .unwrap();

        let started = Instant::now();
        let outcome = store
            .wait(Some(&subscription_id), Duration::from_secs(2))
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        let WaitOutcome::Event(event) = outcome else {
            panic!("expected event, got {outcome:?}");
        };
        assert_eq!(event.task_id.as_deref(), Some("tk_1"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn wait_timeouts_without_event() {
        let root = test_root("wait-timeout");
        let store = EventStore::new(root.clone(), None, None);
        let (subscription_id, _) = store.subscribe(Vec::new(), None).unwrap();
        let outcome = store
            .wait(Some(&subscription_id), Duration::from_millis(40))
            .unwrap();
        assert_eq!(outcome, WaitOutcome::Timeout);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn kinds_filter_is_applied() {
        let root = test_root("kind-filter");
        let store = EventStore::new(root.clone(), None, None);
        let (subscription_id, _) = store
            .subscribe(vec!["task_failed".to_string()], None)
            .unwrap();
        store
            .append_for_test(event("evt_issue", 10, "issue", Some("thread-a"), None))
            .unwrap();
        assert_eq!(
            store
                .wait(Some(&subscription_id), Duration::from_millis(40))
                .unwrap(),
            WaitOutcome::Timeout
        );
        store
            .append_for_test(event("evt_failed", 20, "task_failed", None, Some("tk_2")))
            .unwrap();
        let WaitOutcome::Event(event) = store
            .wait(Some(&subscription_id), Duration::from_millis(500))
            .unwrap()
        else {
            panic!("expected task_failed");
        };
        assert_eq!(event.kind, "task_failed");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn list_is_sorted_desc_and_limit_is_enforced() {
        let root = test_root("list");
        let store = EventStore::new(root.clone(), None, None);
        for index in 0..105 {
            store
                .append_for_test(event(
                    &format!("evt_{index:03}"),
                    100 + index,
                    "issue",
                    None,
                    None,
                ))
                .unwrap();
        }
        let listed = store.list(2).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "evt_104");
        assert_eq!(listed[1].id, "evt_103");
        let error = call_tool(&store, "events_list", &json!({ "limit": 101 })).unwrap_err();
        assert!(matches!(error, ToolCallError::Invalid(_)));

        let listed = store.list(100).unwrap();
        assert_eq!(listed.len(), 100);
        assert_eq!(listed[0].id, "evt_104");
        assert_eq!(listed[99].id, "evt_005");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task_terminal_transition_is_collected_with_display_name_and_deduped() {
        let root = test_root("tasks");
        let bot_dir = root.join("tasks").join("bot-a");
        fs::create_dir_all(&bot_dir).unwrap();
        fs::write(
            bot_dir.join("tasks.json"),
            r#"[{"id":"tk_demo","name":"发布检查"}]"#,
        )
        .unwrap();
        fs::write(
            bot_dir.join("tasks-state.json"),
            r#"{"tk_demo":{"kind":"failed","finished_at":1234,"last_error":"编译失败"}}"#,
        )
        .unwrap();

        let store = EventStore::new(root.clone(), None, None);
        assert_eq!(store.refresh().unwrap(), 1);
        assert_eq!(store.refresh().unwrap(), 0);
        let events = store.list(10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "task_failed");
        assert_eq!(events[0].task_id.as_deref(), Some("tk_demo"));
        assert_eq!(events[0].payload["display_name"], "发布检查");
        assert_eq!(events[0].payload["last_error"], "编译失败");
        assert!(events[0].payload.get("runtime").is_none());
        assert!(events[0].payload.get("bot_key").is_none());
        assert!(events[0].summary.contains("编译失败"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn walgit_entry_is_collected_and_deduped() {
        let root = test_root("walgit");
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        write_walgit_entry(
            &repo,
            r#"{
                "version":1,
                "kind":"status",
                "id":"thread-demo",
                "actor":"abb-worker-6",
                "ts":2222,
                "parent":"",
                "body":{"status":"needs-review","branch":"feat/demo","work":"等待评审"}
            }"#,
        );

        let store = EventStore::new(root.clone(), Some(repo), None);
        assert_eq!(store.refresh().unwrap(), 1);
        assert_eq!(store.refresh().unwrap(), 0);
        let events = store.list(10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "status_needs_review");
        assert_eq!(events[0].thread.as_deref(), Some("thread-demo"));
        assert_eq!(events[0].payload["branch"], "feat/demo");
        assert_eq!(events[0].payload["status"], "needs-review");
        assert!(events[0].payload.get("body").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn walgit_body_is_not_persisted() {
        let root = test_root("walgit-privacy");
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        write_walgit_entry(
            &repo,
            r#"{"version":1,"kind":"issue","id":"privacy-thread","actor":"a","ts":9,"body":{"title":"safe title","note":"FAKE_NOTE_PLACEHOLDER","prompt":"FAKE_PROMPT_PLACEHOLDER"}}"#,
        );
        let store = EventStore::new(root.clone(), Some(repo), None);
        assert_eq!(store.refresh().unwrap(), 1);
        let persisted = fs::read_to_string(store.events_path()).unwrap();
        assert!(!persisted.contains("FAKE_NOTE_PLACEHOLDER"));
        assert!(!persisted.contains("FAKE_PROMPT_PLACEHOLDER"));
        assert!(!persisted.contains("\"body\""));
        let events = store.list(10).unwrap();
        assert!(events[0].payload.get("body").is_none());
        assert!(events[0].summary.chars().count() <= MAX_EVENT_TEXT_CHARS);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn task_runtime_is_reduced_and_last_error_is_bounded() {
        let root = test_root("task-privacy");
        let bot_dir = root.join("tasks").join("bot-a");
        fs::create_dir_all(&bot_dir).unwrap();
        fs::write(
            bot_dir.join("tasks.json"),
            r#"[{"id":"tk_secret","name":"敏感任务"}]"#,
        )
        .unwrap();
        let long_error = "错误".repeat(300);
        let state = json!({
            "tk_secret": {
                "kind": "failed",
                "finished_at": 42,
                "last_error": long_error,
                "runtime_secret": "FAKE_RUNTIME_PLACEHOLDER"
            }
        });
        fs::write(
            bot_dir.join("tasks-state.json"),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();

        let store = EventStore::new(root.clone(), None, None);
        assert_eq!(store.refresh().unwrap(), 1);
        let persisted = fs::read_to_string(store.events_path()).unwrap();
        assert!(!persisted.contains("FAKE_RUNTIME_PLACEHOLDER"));
        let events = store.list(10).unwrap();
        let event = &events[0];
        assert!(event.payload.get("runtime").is_none());
        assert!(event.payload.get("bot_key").is_none());
        assert!(
            event.payload["last_error"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= MAX_EVENT_TEXT_CHARS
        );
        assert!(event.summary.chars().count() <= MAX_EVENT_TEXT_CHARS);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn truncated_tail_is_repaired_before_new_event_append() {
        let root = test_root("truncated-tail");
        let store = EventStore::new(root.clone(), None, None);
        let mut first = event("evt_old", 1, "issue", None, None);
        first.payload = json!({ "blob": "x".repeat(1000) });
        store.append_for_test(first).unwrap();

        let path = store.events_path();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(497).unwrap();
        drop(file);

        store
            .append_for_test(event("evt_new", 2, "issue", None, None))
            .unwrap();
        let listed = store.list(10).unwrap();
        assert!(
            listed.iter().any(|event| event.id == "evt_new"),
            "new event must survive a truncated tail; listed={listed:?}"
        );
        let persisted = fs::read_to_string(&path).unwrap();
        assert!(persisted.ends_with('\n'));
        for line in persisted.lines() {
            assert!(
                serde_json::from_str::<Event>(line).is_ok(),
                "event file contains a malformed line: {line:?}"
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn events_rotate_at_five_mib_and_drop_oldest_segment() {
        let root = test_root("rotate");
        let store = EventStore::new(root.clone(), None, None);
        let marker = "OLDEST_SEGMENT_MARKER";
        for index in 0..6 {
            let mut event = event(&format!("evt_rotate_{index}"), index, "issue", None, None);
            if index == 0 {
                event.payload = json!({ "marker": marker, "blob": "x".repeat(1_000_000) });
            } else {
                event.payload = json!({ "blob": "x".repeat(1_000_000) });
            }
            store.append_for_test(event).unwrap();
        }
        let current_len = fs::metadata(store.events_path()).unwrap().len();
        assert!(current_len <= EVENTS_MAX_BYTES);
        assert!(rotated_path(&store.events_path(), 1).exists());
        store
            .with_lock(|| {
                store.rotate_events_unlocked()?;
                store.rotate_events_unlocked()?;
                Ok(())
            })
            .unwrap();
        let segment_2 = fs::read_to_string(rotated_path(&store.events_path(), 2)).unwrap();
        assert!(!segment_2.contains(marker));
        assert!(segment_2.len() as u64 <= EVENTS_MAX_BYTES);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unconfigured_repo_returns_explicit_warning() {
        let root = test_root("unconfigured");
        let store = EventStore::new(root.clone(), None, None);
        let result = call_tool(&store, "events_list", &json!({})).unwrap();
        let warnings = result["warnings"].as_array().unwrap();
        assert!(!warnings.is_empty());
        assert!(warnings.iter().any(|warning| warning
            .as_str()
            .is_some_and(|text| text.contains("未配置事件仓库"))));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn walgit_patch_review_and_issue_criteria_match_watch_notify() {
        let root = test_root("walgit-criteria");
        let repo = root.join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        write_walgit_entry(
            &repo,
            r#"{"version":1,"kind":"issue","id":"t1","actor":"a","ts":1,"body":{"title":"new"}}"#,
        );
        write_walgit_entry(
            &repo,
            r#"{"version":1,"kind":"patch","id":"t2","actor":"a","ts":2,"refs":{"base":"refs/heads/main","head":"refs/heads/feat/x"},"body":{"title":"patch"}}"#,
        );
        write_walgit_entry(
            &repo,
            r#"{"version":1,"kind":"review","id":"t3","actor":"r","ts":3,"body":{"decision":"needs-changes","note":"fix"}}"#,
        );
        write_walgit_entry(
            &repo,
            r#"{"version":1,"kind":"status","id":"t4","actor":"a","ts":4,"body":{"status":"closed","work":"ignore"}}"#,
        );

        let collectors = EventCollectors {
            tasks_root: root.join("tasks"),
            repo: Some(repo),
            remote: None,
            walgit_last_fetch: Mutex::new(None),
            walgit_seen_oids: Mutex::new(BTreeSet::new()),
        };
        let kinds: BTreeSet<_> = collectors
            .collect_walgit(&BTreeSet::new())
            .into_iter()
            .map(|event| event.kind)
            .collect();
        assert!(kinds.contains("issue"));
        assert!(kinds.contains("patch"));
        assert!(kinds.contains("review_needs_changes"));
        assert!(!kinds.iter().any(|kind| kind == "status_closed"));
        let _ = fs::remove_dir_all(root);
    }
}
