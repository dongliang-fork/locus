//! One SSH stdio connection carries PTY bytes, Mod events and local tool replies.
//! This module never sends inference requests or forwards a network port.
use super::mod_bridge::PendingCall;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;
pub(crate) mod unlock;
mod archive_rpc;
mod heartbeat;

const MAX_FRAME: usize = 32 * 1024 * 1024;
const PYTHON: &str = "exec python3 -u -c 'import base64,sys;exec(compile(base64.b64decode(sys.stdin.buffer.readline()),\"<locus-ssh>\",\"exec\"))'";
const INSTALL_PYTHON: &str = "if ! command -v python3 >/dev/null 2>&1; then if [ \"$(id -u)\" = 0 ]; then SUDO=''; elif sudo -n true 2>/dev/null; then SUDO='sudo -n'; else echo 'Python 3 is missing; installation requires passwordless sudo or an administrator' >&2; exit 1; fi; if command -v apt-get >/dev/null 2>&1; then $SUDO apt-get update >&2 && $SUDO apt-get install -y python3 bash ca-certificates curl >&2 || exit 1; elif command -v dnf >/dev/null 2>&1; then $SUDO dnf install -y python3 bash ca-certificates curl >&2 || exit 1; elif command -v apk >/dev/null 2>&1; then $SUDO apk add python3 bash ca-certificates curl >&2 || exit 1; else echo 'Install Python 3 on this server first' >&2; exit 1; fi; fi; ";
const CHECK_PYTHON: &str = "if ! command -v python3 >/dev/null 2>&1; then printf '%s\\n' '{\"type\":\"hello\",\"protocol\":1,\"info\":{\"pythonInstalled\":false,\"cliSupported\":false,\"authenticated\":false}}'; exit 0; fi; ";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SshConfig {
    /// An OpenSSH config alias or user@hostname. Never a shell command.
    pub target: String,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
    pub known_hosts_file: Option<String>,
    pub cli_path: Option<String>,
}
impl SshConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.target.is_empty()
            || self.target.starts_with('-')
            || self.target.len() > 512
            || !self
                .target
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_.-@[]:%".contains(&c))
            || self.target.matches('@').count() > 1
            || self.port == Some(0)
        {
            return Err(
                "Enter an SSH alias or user@hostname, with the port in its own field".into(),
            );
        }
        for value in [&self.identity_file, &self.known_hosts_file, &self.cli_path]
            .into_iter()
            .flatten()
        {
            if value.is_empty() || value.chars().any(char::is_control) {
                return Err("SSH paths must not be empty or contain control characters".into());
            }
        }
        Ok(())
    }
    pub fn fingerprint(&self) -> String {
        uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            &serde_json::to_vec(self).expect("SSH config serialization"),
        )
        .to_string()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteInfo {
    #[serde(default, skip_serializing)]
    pub history: Option<Value>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub keychain_unlocked: Option<bool>,
    #[serde(default)]
    pub keychain_unlock_tested: bool,
    pub python_installed: bool,
    pub cli_version: Option<String>,
    pub cli_supported: bool,
    pub model: Option<String>,
    pub authenticated: bool,
    pub home: Option<String>,
    pub session_root: Option<String>,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    #[default]
    Auto,
    BypassPermissions,
}
impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionSettings {
    pub version: u32,
    #[serde(default)]
    pub permission_mode: PermissionMode,
    #[serde(flatten)]
    pub runtime: super::runtime_settings::RuntimeSettings,
    #[serde(default)]
    pub prewarm: bool,
    #[serde(default = "default_pool_capacity")]
    pub pool_capacity: u32,
    #[serde(default = "default_idle_minutes")]
    pub idle_minutes: u32,
    pub remote: Option<SshConfig>,
    pub authorized_target: Option<String>,
    pub info: Option<RemoteInfo>,
    /// Computed on read; never accept a frontend claim that a password exists.
    #[serde(default, skip_deserializing)]
    pub has_unlock_password: bool,
}
impl Default for ConnectionSettings {
    fn default() -> Self {
        Self {
            version: 1,
            permission_mode: PermissionMode::default(),
            runtime: Default::default(),
            prewarm: false,
            pool_capacity: default_pool_capacity(),
            idle_minutes: default_idle_minutes(),
            remote: None,
            authorized_target: None,
            info: None,
            has_unlock_password: false,
        }
    }
}
fn default_idle_minutes() -> u32 { 15 }
fn default_pool_capacity() -> u32 { 1 }
fn settings_path() -> Result<PathBuf, String> {
    Ok(crate::commands::persistent_config_dir()?.join("claude_connection.json"))
}
pub fn load_settings() -> Result<ConnectionSettings, String> {
    let path = settings_path()?;
    match std::fs::read(&path) {
        Ok(bytes) => {
            let value: ConnectionSettings =
                serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            if value.version != 1 {
                return Err("Unsupported Claude connection configuration version".into());
            }
            value.runtime.validate()?;
            if let Some(remote) = &value.remote {
                remote.validate()?;
                if value.authorized_target.as_deref() != Some(remote.fingerprint().as_str()) {
                    return Err(
                        "Authorize environment setup for this SSH target in Settings".into(),
                    );
                }
            }
            Ok(value)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ConnectionSettings::default()),
        Err(e) => Err(e.to_string()),
    }
}
pub fn save_settings(value: &ConnectionSettings) -> Result<(), String> {
    value.runtime.validate()?;
    write_json(&settings_path()?, value)
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    std::fs::create_dir_all(path.parent().ok_or("Missing parent directory")?)
        .map_err(|e| e.to_string())?;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec(value).map_err(|e| e.to_string())?)
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    drop(file);
    // Windows rename replaces existing regular files as well.
    std::fs::rename(temporary, path).map_err(|e| e.to_string())
}
pub fn profile_root(id: &str) -> Result<PathBuf, String> {
    Ok(std::env::var_os("LOCUS_CLAUDE_TUI_PROFILE_ROOT")
        .map(PathBuf::from)
        .unwrap_or(crate::commands::persistent_config_dir()?.join("claude-tui"))
        .join(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, id.as_bytes()).to_string()))
}
/// Only called with an explicit, persisted restore binding from SessionStore.
pub(crate) fn bind_restored_connection(id:&str,remote:&SshConfig)->Result<(),String> {
    remote.validate()?;
    let path=profile_root(id)?.join("connection.json");
    let value=json!({"version":1,"remote":remote});
    if std::fs::read(&path).ok().and_then(|bytes|serde_json::from_slice::<Value>(&bytes).ok()).as_ref()!=Some(&value) {
        write_json(&path,&value)?;
    }
    Ok(())
}
/// Explicit, repeatable v1 migration: historical local sessions remain local.
/// A saved binding wins over subsequently changed global connection settings.
pub fn session_connection(
    id: &str,
    legacy_local: bool,
    recorded: Option<Option<SshConfig>>,
) -> Result<Option<SshConfig>, String> {
    static LOCK: Mutex<()> = Mutex::new(());
    let _lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = profile_root(id)?.join("connection.json");
    if let Some(recorded) = recorded {
        // The database survives profile-directory loss and participates in
        // normal session backup/export. Never reinterpret a remote resume ID
        // as local merely because its configuration sidecar is missing.
        if let Some(config) = &recorded {
            config.validate()?;
        }
        let bound = resolve_binding(&path, false, || Ok(recorded.clone()))?;
        if bound != recorded {
            return Err("Claude session connection differs from its saved history".into());
        }
        Ok(bound)
    } else {
        resolve_binding(&path, legacy_local, || load_settings().map(|s| s.remote))
    }
}
fn resolve_binding(
    path: &Path,
    legacy_local: bool,
    current: impl FnOnce() -> Result<Option<SshConfig>, String>,
) -> Result<Option<SshConfig>, String> {
    #[derive(Serialize, Deserialize)]
    struct Binding {
        version: u32,
        remote: Option<SshConfig>,
    }
    match std::fs::read(path) {
        Ok(bytes) => {
            let binding: Binding = serde_json::from_slice(&bytes)
                .map_err(|e| format!("Invalid Claude session connection: {e}"))?;
            if binding.version != 1 {
                return Err("Unsupported Claude session connection version".into());
            }
            if let Some(remote) = &binding.remote {
                remote.validate()?;
            }
            Ok(binding.remote)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let remote = if legacy_local { None } else { current()? };
            write_json(
                path,
                &Binding {
                    version: 1,
                    remote: remote.clone(),
                },
            )?;
            Ok(remote)
        }
        Err(e) => Err(e.to_string()),
    }
}
pub fn session_key(id: &str, remote: &SshConfig) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("{id}:{}", remote.fingerprint()).as_bytes(),
    )
    .to_string()
}

pub struct Transport {
    #[cfg(windows)]
    job: super::pty_job::PtyJob,
    child: Mutex<tokio::process::Child>,
    send: mpsc::Sender<Value>,
    healthy: Arc<AtomicBool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    pub diagnostic: Arc<Mutex<Vec<u8>>>,
    failure: Arc<Mutex<Option<String>>>,
    archive_rpc: Arc<archive_rpc::ArchiveRpc>,
    heartbeat: Arc<Mutex<heartbeat::Heartbeat>>,
}
impl Transport {
    pub fn failure(&self) -> String {
        self.failure.lock().unwrap_or_else(|e| e.into_inner()).clone()
            .unwrap_or_else(|| super::turn_retry::disconnected("SSH connection closed"))
    }
    pub fn latency_ms(&self) -> Option<f64> {
        if !self.alive() { return None; }
        self.heartbeat.lock().unwrap_or_else(|e| e.into_inner()).latency_ms(std::time::Instant::now())
    }
    pub fn alive(&self) -> bool {
        self.healthy.load(Ordering::SeqCst)
            && matches!(
                self.child
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .try_wait(),
                Ok(None)
            )
    }
    pub fn stop(&self) {
        self.archive_rpc.close(&self.failure());
        self.healthy.store(false, Ordering::SeqCst);
        let _ = self.send.try_send(json!({"type":"close"}));
        #[cfg(windows)]
        self.job.terminate();
        let _ = self
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .start_kill();
        // A stopped Session can remain in the cache until its next turn. Do not
        // retain its writer/reader tasks (or pending tool futures) until Drop.
        for task in &self.tasks {
            task.abort();
        }
    }
    pub fn send(&self, value: Value) -> Result<(), String> {
        if !self.alive() {
            return Err(self.failure());
        }
        self.send
            .try_send(value)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Closed(_) => self.failure(),
                mpsc::error::TrySendError::Full(_) => "SSH input queue is full".into(),
            })
    }
    pub fn input(&self, text: &str) -> Result<(), String> {
        self.send(
            json!({"type":"input","data":base64::engine::general_purpose::STANDARD.encode(text)}),
        )
    }
    pub async fn start(
        config: &SshConfig,
        request: Value,
        calls: Option<(mpsc::Sender<PendingCall>, Arc<super::mod_bridge::ControlSignals>, Arc<std::sync::RwLock<Value>>)>,
    ) -> Result<(Self, RemoteInfo), String> {
        Self::start_with_unlock(config, request, calls, unlock::UnlockPassword::Saved).await
    }

    pub async fn start_with_unlock(
        config: &SshConfig,
        mut request: Value,
        calls: Option<(mpsc::Sender<PendingCall>, Arc<super::mod_bridge::ControlSignals>, Arc<std::sync::RwLock<Value>>)>,
        unlock_password: unlock::UnlockPassword,
    ) -> Result<(Self, RemoteInfo), String> {
        config.validate()?;
        let unlock_key = unlock::key(config)?;
        if request.get("runtimeSettings").is_none() {
            request["runtimeSettings"] = json!(load_settings()?.runtime);
        }
        request["protocol"] = json!(1);
        request["cliPath"] = json!(config.cli_path);
        request["keychainUnlock"] = json!(true);
        let authorized = request["authorizedSetup"] == true;
        let mut command = tokio::process::Command::new("ssh");
        command.args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=15",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
        ]);
        if let Some(port) = config.port {
            command.args(["-p", &port.to_string()]);
        }
        if let Some(path) = &config.identity_file {
            command.args(["-i", path]);
        }
        if let Some(path) = &config.known_hosts_file {
            command.args(["-o", &format!("UserKnownHostsFile={path}")]);
        }
        command.arg("--").arg(&config.target).arg(format!(
            "{}{}",
            if authorized {
                INSTALL_PYTHON
            } else {
                CHECK_PYTHON
            },
            PYTHON
        ));
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(super::CREATE_NO_WINDOW);
        let mut child = command
            .spawn()
            .map_err(|e| format!("Could not start OpenSSH: {e}"))?;
        #[cfg(windows)]
        let job = super::pty_job::PtyJob::attach(child.id().ok_or("SSH process ID unavailable")?)?;
        let mut stdin = child.stdin.take().ok_or("SSH stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("SSH stdout unavailable")?;
        let mut stderr = child.stderr.take().ok_or("SSH stderr unavailable")?;
        let (send, mut receive) = mpsc::channel::<Value>(64);
        // Separate from ordinary frames so passwords cannot enter generic logs
        // or be retained as session input. The writer wipes its serialized bytes.
        let (unlock_send, mut unlock_receive) = mpsc::channel::<Option<Zeroizing<String>>>(1);
        let healthy = Arc::new(AtomicBool::new(true));
        let failure = Arc::new(Mutex::new(None));
        let diagnostic = Arc::new(Mutex::new(Vec::new()));
        let error_buffer = diagnostic.clone();
        let stderr_task = tokio::spawn(async move {
            let mut bytes = [0; 8192];
            while let Ok(count) = stderr.read(&mut bytes).await {
                if count == 0 {
                    break;
                }
                append_diagnostic(&error_buffer, &bytes[..count]);
            }
        });
        let writer_health = healthy.clone();
        let writer_failure = failure.clone();
        let heartbeat = Arc::new(Mutex::new(heartbeat::Heartbeat::default()));
        let writer_heartbeat = heartbeat.clone();
        let source = base64::engine::general_purpose::STANDARD.encode(include_str!("ssh_host.py"));
        let write_task = tokio::spawn(async move {
            let run = async {
                stdin
                    .write_all(format!("{source}\n{request}\n").as_bytes())
                    .await?;
                stdin.flush().await?;
                loop {
                    let bytes = tokio::select! {
                        Some(password) = unlock_receive.recv() => {
                            #[derive(Serialize)]
                            struct Reply<'a> { r#type: &'static str, password: Option<&'a str> }
                            Zeroizing::new(serde_json::to_vec(&Reply {
                                r#type: "keychain_unlock", password: password.as_deref().map(String::as_str),
                            })?)
                        }
                        value = receive.recv() => {
                            let Some(value) = value else { break; };
                            if value["type"] == "ping" {
                                if let Some(id) = value["id"].as_u64() {
                                    writer_heartbeat.lock().unwrap_or_else(|e| e.into_inner()).sent(id, std::time::Instant::now());
                                }
                            }
                            Zeroizing::new(serde_json::to_vec(&value)?)
                        }
                    };
                    if bytes.len() > MAX_FRAME {
                        return Err(std::io::Error::other("SSH frame too large"));
                    }
                    stdin.write_all(&bytes).await?;
                    stdin.write_all(b"\n").await?;
                    stdin.flush().await?;
                }
                Ok::<_, std::io::Error>(())
            };
            if let Err(error) = run.await {
                writer_failure.lock().unwrap_or_else(|e| e.into_inner())
                    .get_or_insert_with(|| super::turn_retry::disconnected(format!("SSH write failed: {error}")));
            }
            writer_health.store(false, Ordering::SeqCst);
        });
        let (hello_send, hello_receive) = oneshot::channel();
        let read_health = healthy.clone();
        let read_failure = failure.clone();
        let reply_send = send.clone();
        let read_buffer = diagnostic.clone();
        let archive_rpc=Arc::new(archive_rpc::ArchiveRpc::default());
        let read_archive_rpc=archive_rpc.clone();
        let read_heartbeat = heartbeat.clone();
        let read_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut hello = Some(hello_send);
            let mut unlock_requested = false;
            let mut pending = tokio::task::JoinSet::new();
            loop {
                while pending.try_join_next().is_some() {}
                // Reading a frame is deliberately not raced against completed
                // replies: cancelling a partial read would discard its prefix.
                let value = match read_frame(&mut reader).await {
                    Ok(Some(value)) => value,
                    result => {
                        let error = result.err().unwrap_or_else(|| super::turn_retry::disconnected("SSH output closed"));
                        read_failure.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(error);
                        break;
                    }
                };
                if read_archive_rpc.route(&value) { continue; }
                match value["type"].as_str() {
                    Some("keychain_unlock_required") if hello.is_some()
                        && !unlock_requested && value["protocol"] == 1 && value["platform"] == "darwin" => {
                        unlock_requested = true;
                        // A Linux peer never asks for this and never loads the local secret.
                        match unlock_password.resolve(&unlock_key) {
                            Ok(password) => { if unlock_send.send(password).await.is_err() { break; } }
                            Err(_) => {
                                if let Some(hello) = hello.take() {
                                    let _ = hello.send(Err("Could not read the saved macOS keychain password from this computer's credential store".into()));
                                }
                                break;
                            }
                        }
                    }
                    Some("hello") if value["protocol"] == 1 => {
                        if let Some(hello) = hello.take() {
                            let _ = hello.send(
                                serde_json::from_value::<RemoteInfo>(value["info"].clone())
                                    .map_err(|e| e.to_string()),
                            );
                        }
                    }
                    Some("terminal") => {
                        if let Some(data) = value["data"]
                            .as_str()
                            .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
                        {
                            append_diagnostic(&read_buffer, &data);
                            if data.windows(4).any(|p| p == b"\x1b[6n") {
                                let _ = reply_send.try_send(json!({"type":"input","data":base64::engine::general_purpose::STANDARD.encode(b"\x1b[1;1R")}));
                            }
                        }
                    }
                    Some("bridge_health") => {
                        // One-way samples bypass tool queues and pending reply
                        // tasks. No remote receipt is needed for liveness: the
                        // lease advances only when this process receives it.
                        if value["payload"]["type"] == "bridge_health" {
                            if let Some((_, controls, options)) = &calls {
                                controls.route(&value["payload"], options);
                            }
                        }
                    }
                    Some("call") => {
                        let Some(calls) = calls.clone().filter(|_| pending.len() < 64) else {
                            read_failure.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert("Unexpected or excessive SSH bridge calls".into());
                            break;
                        };
                        let Some(id) = value["id"].as_str().map(str::to_string) else {
                            read_failure.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert("SSH bridge call has no identity".into());
                            break;
                        };
                        let send = reply_send.clone();
                        pending.spawn(async move {
                            let (calls,controls,options)=calls;
                            if let Some(result)=controls.route(&value["payload"],&options) {
                                let _=send.send(json!({"type":"reply","id":id,"value":result})).await;
                                return;
                            }
                            let (reply, receive) = oneshot::channel();
                            if calls
                                .send(PendingCall {
                                    payload: value["payload"].clone(),
                                    reply,
                                    received_at: tokio::time::Instant::now(),
                                })
                                .await
                                .is_err()
                            {
                                return;
                            }
                            let result = receive.await.unwrap_or_else(
                                |_| json!({"bridgeError":"Locus tool handler closed"}),
                            );
                            let _ = send
                                .send(json!({"type":"reply","id":id,"value":result}))
                                .await;
                        });
                    }
                    Some("error") => {
                        let message = value["message"]
                            .as_str()
                            .unwrap_or("Remote Claude failed")
                            .to_string();
                        append_diagnostic(&read_buffer, message.as_bytes());
                        read_failure.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert(message.clone());
                        if let Some(hello) = hello.take() {
                            let _ = hello.send(Err(message));
                        }
                        break;
                    }
                    Some("exit") => {
                        read_failure.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(||
                            super::turn_retry::disconnected(format!("Remote Claude exited: {value}")));
                        break;
                    },
                    Some("pong") => {
                        if let Some(id) = value["id"].as_u64() {
                            read_heartbeat.lock().unwrap_or_else(|e| e.into_inner()).received(id, std::time::Instant::now());
                        }
                    }
                    Some("progress") => {}
                    _ => {
                        read_failure.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert("Unexpected SSH protocol frame".into());
                        break;
                    },
                }
            }
            read_health.store(false, Ordering::SeqCst);
            let error = read_failure.lock().unwrap_or_else(|e| e.into_inner()).clone()
                .unwrap_or_else(|| super::turn_retry::disconnected("SSH reader stopped"));
            if calls.is_some() { eprintln!("[Claude] transport ended: {error}"); }
            read_archive_rpc.close(&error);
        });
        let heartbeat_health = healthy.clone();
        let heartbeat_send = send.clone();
        let heartbeat_task = tokio::spawn(async move {
            let mut id = 0_u64;
            while heartbeat_health.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(5)).await;
                id += 1;
                if heartbeat_send.send(json!({"type":"ping","id":id})).await.is_err() {
                    break;
                }
            }
        });
        let mut transport = Self {
            archive_rpc,
            heartbeat,
            #[cfg(windows)]
            job,
            child: Mutex::new(child),
            send,
            healthy,
            tasks: vec![read_task, write_task, stderr_task, heartbeat_task],
            diagnostic,
            failure,
        };
        let info = match tokio::time::timeout(
            Duration::from_secs(if authorized { 660 } else { 45 }),
            hello_receive,
        )
        .await
        {
            Ok(Ok(result)) => result?,
            _ => {
                // stdout EOF can precede delivery of OpenSSH's stderr verdict.
                // Drain it briefly so permission/host-key failures are not
                // mislabeled as an unexplained bridge closure.
                let _ = tokio::time::timeout(Duration::from_millis(200), &mut transport.tasks[2]).await;
                let diagnostic = transport.diagnostic.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let error = if diagnostic.is_empty() {
                    transport.failure.lock().unwrap_or_else(|e| e.into_inner()).clone()
                        .unwrap_or_else(|| "SSH environment check timed out".into())
                } else {
                    format!("SSH environment check failed: {}", String::from_utf8_lossy(&diagnostic))
                };
                return Err(super::turn_retry::ssh_startup_error(error));
            }
        };
        Ok((transport, info))
    }
}
impl Drop for Transport {
    fn drop(&mut self) {
        self.stop();
    }
}
pub async fn probe(config: &SshConfig, authorized: bool) -> Result<RemoteInfo, String> {
    probe_with_unlock(config, authorized, unlock::UnlockPassword::Saved).await
}
pub async fn probe_with_unlock(config: &SshConfig, authorized: bool, password: unlock::UnlockPassword) -> Result<RemoteInfo, String> {
    let (_transport, info) = Transport::start_with_unlock(
        config,
        json!({"mode":if authorized {"prepare"} else {"inspect"},"authorizedSetup":authorized}),
        None,
        password,
    )
    .await?;
    Ok(info)
}
fn append_diagnostic(buffer: &Mutex<Vec<u8>>, bytes: &[u8]) {
    let mut buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
    buffer.extend_from_slice(bytes);
    if buffer.len() > 65536 {
        let excess = buffer.len() - 65536;
        buffer.drain(..excess);
    }
}
async fn read_frame(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> Result<Option<Value>, String> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(|e| super::turn_retry::disconnected(format!("SSH read failed: {e}")))?;
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(super::turn_retry::disconnected("Truncated SSH frame"))
            };
        }
        let count = available
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(available.len());
        if bytes.len() + count > MAX_FRAME {
            return Err("SSH frame exceeds size limit".into());
        }
        let complete = available[count - 1] == b'\n';
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if complete {
            return serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| e.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_mode_defaults_to_auto_and_roundtrips_explicit_bypass() {
        let old = json!({"version":1,"remote":null,"authorizedTarget":null,"info":null});
        let mut settings: ConnectionSettings = serde_json::from_value(old).unwrap();
        assert_eq!(settings.permission_mode, PermissionMode::Auto);
        assert!(!settings.prewarm);
        assert_eq!(settings.runtime, super::super::runtime_settings::RuntimeSettings::default());
        settings.runtime.idle_compaction = true;
        settings.runtime.telemetry = true;
        settings.runtime.auto_compact_window = Some(350_000);
        assert_eq!((settings.pool_capacity, settings.idle_minutes), (1, 15));
        assert_eq!(ConnectionSettings::default().permission_mode.as_str(), "auto");
        settings.permission_mode = PermissionMode::BypassPermissions;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("connection.json");
        write_json(&path, &settings).unwrap();
        let saved: ConnectionSettings = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved.permission_mode.as_str(), "bypassPermissions");
        assert_eq!(saved.runtime, settings.runtime);
        assert_eq!(serde_json::to_value(&saved).unwrap()["idleCompaction"], true);
        assert_eq!(serde_json::to_value(&saved).unwrap()["telemetry"], true);
        assert_eq!(serde_json::to_value(&saved).unwrap()["autoCompactWindow"], 350_000);
        assert!(serde_json::from_value::<PermissionMode>(json!("unknown")).is_err());
    }
    fn config() -> SshConfig {
        SshConfig {
            target: "user@host".into(),
            port: None,
            identity_file: None,
            known_hosts_file: None,
            cli_path: None,
        }
    }
    #[test]
    fn ssh_targets_cannot_inject_options_or_shell() {
        for target in [
            "-oProxyCommand=evil",
            "user@host;id",
            "$(id)",
            "host\n",
            "host path",
        ] {
            assert!(SshConfig {
                target: target.into(),
                ..config()
            }
            .validate()
            .is_err());
        }
        assert!(config().validate().is_ok());
        assert_ne!(
            config().fingerprint(),
            SshConfig {
                port: Some(2222),
                ..config()
            }
            .fingerprint()
        );
    }
    #[test]
    fn connection_binding_is_versioned_pinned_and_migrates_local() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("old.json");
        assert_eq!(
            resolve_binding(&old, true, || Ok(Some(config()))).unwrap(),
            None
        );
        assert_eq!(
            resolve_binding(&old, false, || panic!("must stay local")).unwrap(),
            None
        );
        let new = root.path().join("new.json");
        assert_eq!(
            resolve_binding(&new, false, || Ok(Some(config()))).unwrap(),
            Some(config())
        );
        assert_eq!(
            resolve_binding(&new, true, || panic!("must stay remote")).unwrap(),
            Some(config())
        );
        std::fs::write(&new, r#"{"version":2,"remote":null}"#).unwrap();
        assert!(resolve_binding(&new, false, || Ok(None)).is_err());
    }
    #[tokio::test]
    async fn frames_reject_truncation_and_invalid_json() {
        assert_eq!(
            read_frame(&mut &b"{\"type\":\"pong\"}\n"[..])
                .await
                .unwrap()
                .unwrap()["type"],
            "pong"
        );
        let truncated = read_frame(&mut &b"{}"[..]).await.unwrap_err();
        assert!(super::super::turn_retry::is_disconnected(&truncated));
        let malformed = read_frame(&mut &b"oops\n"[..]).await.unwrap_err();
        assert!(!super::super::turn_retry::is_disconnected(&malformed));
    }
}
