//! Native desktop process ownership and the optional Linux capture worker.
use crate::DesktopJournal;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};
use zeroize::Zeroizing;

pub const DAEMON_FLAG: &str = "--memorious-daemon";
pub fn socket(dir: &Path) -> PathBuf {
    dir.join(".memorious-daemon.sock")
}
pub fn control_socket(dir: &Path) -> PathBuf {
    dir.join(".memorious-ptt-control.sock")
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PttConfig {
    pub enabled: bool,
    pub key: String,
    pub api_key: String,
    pub stt_model: String,
    pub cleanup_model: String,
    #[serde(default = "remote_provider")]
    pub provider: String,
    #[serde(default = "auto_language")]
    pub language: String,
}
fn auto_language() -> String {
    "auto".into()
}
fn remote_provider() -> String {
    "remote".into()
}
impl Default for PttConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            key: "KEY_RIGHTCTRL".into(),
            api_key: String::new(),
            stt_model: "gpt-4o-mini-transcribe".into(),
            cleanup_model: "gpt-4o-mini".into(),
            provider: "local".into(),
            language: auto_language(),
        }
    }
}
pub fn load_ptt(dir: &Path) -> Result<PttConfig> {
    let path = dir.join(".memorious-ptt.json");
    if !path.exists() {
        return Ok(PttConfig::default());
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
pub fn save_ptt(dir: &Path, enabled: bool, key: &str, api_key: Option<&str>) -> Result<()> {
    let provider = load_ptt(dir)?.provider;
    save_ptt_with_provider(dir, enabled, key, api_key, &provider)
}
pub fn save_ptt_with_provider(
    dir: &Path,
    enabled: bool,
    key: &str,
    api_key: Option<&str>,
    provider: &str,
) -> Result<()> {
    let language = load_ptt(dir)?.language;
    save_ptt_options(dir, enabled, key, api_key, provider, &language)
}
pub fn save_ptt_options(
    dir: &Path,
    enabled: bool,
    key: &str,
    api_key: Option<&str>,
    provider: &str,
    language: &str,
) -> Result<()> {
    let language = language.trim().to_ascii_lowercase();
    if language != "auto"
        && (!(2..=3).contains(&language.len()) || !language.bytes().all(|c| c.is_ascii_lowercase()))
    {
        bail!("Use auto or a language code such as af, en, or es");
    }
    if !matches!(provider, "local" | "remote") {
        bail!("Select local or remote transcription");
    }
    if !key.starts_with("KEY_") || key.len() > 64 {
        bail!("Choose a Linux key name, for example KEY_RIGHTCTRL");
    }
    let mut config = load_ptt(dir)?;
    config.enabled = enabled;
    config.provider = provider.into();
    config.language = language;
    config.key = key.into();
    if let Some(api_key) = api_key.filter(|s| !s.trim().is_empty()) {
        config.api_key = api_key.trim().into();
    }
    if enabled && provider == "remote" && config.api_key.is_empty() {
        bail!("Enter a transcription API key to enable push-to-talk");
    }
    use std::os::unix::fs::OpenOptionsExt;
    let path = dir.join(".memorious-ptt.json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(&serde_json::to_vec(&config)?)?;
    std::fs::rename(path, dir.join(".memorious-ptt.json"))?;
    Ok(())
}
pub fn transcribe_file(dir: &Path, path: &Path) -> Result<String> {
    let executable = std::env::current_exe()?.with_file_name("memorious-ptt");
    let output = Command::new(executable)
        .arg("--data-dir")
        .arg(dir)
        .arg("--transcribe-file")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .context("Transcribe voice recording")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr));
    }
    let value: Value = serde_json::from_slice(&output.stdout)?;
    Ok(value["text"]
        .as_str()
        .context("Missing transcript")?
        .to_owned())
}

pub fn local_model_status() -> Result<Value> {
    let executable = std::env::current_exe()?.with_file_name("memorious-ptt");
    let output = Command::new(executable)
        .arg("--model-status")
        .output()
        .context("Inspect local transcription model")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}
pub fn download_local_model() -> Result<()> {
    let executable = std::env::current_exe()?.with_file_name("memorious-ptt");
    let mut child = Command::new(executable)
        .arg("--download-model")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn ptt_status(dir: &Path) -> Value {
    std::fs::read(dir.join(".memorious-ptt-status.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(json!({"state":"stopped"}))
}
pub fn ptt_command(dir: &Path, command: &str) -> Result<()> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(control_socket(dir))
        .context("Push-to-talk is not running; enable it in background settings")?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(8)))?;
    writeln!(stream, "{command}")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    if response.trim() != "ok" {
        bail!("{response}");
    }
    Ok(())
}

pub struct Session {
    direct: Option<DesktopJournal>,
    dir: PathBuf,
    background: bool,
    password: Zeroizing<String>,
}
impl Session {
    pub fn embedded(dir: PathBuf, password: Zeroizing<String>, journal: DesktopJournal) -> Self {
        Self {
            direct: Some(journal),
            dir,
            background: false,
            password,
        }
    }
    pub async fn create(dir: &Path, password: &str) -> Result<Self> {
        Ok(Self::embedded(
            dir.into(),
            Zeroizing::new(password.into()),
            DesktopJournal::create(dir, password).await?,
        ))
    }
    pub async fn attach(dir: &Path) -> Result<Self> {
        memorious_core::local::call(&socket(dir), "health", json!({})).await?;
        Ok(Self {
            direct: None,
            dir: dir.into(),
            background: true,
            password: Zeroizing::new(String::new()),
        })
    }
    pub fn is_background(&self) -> bool {
        self.background
    }
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if self.background {
            tokio::time::timeout(
                std::time::Duration::from_secs(180),
                memorious_core::local::call(&socket(&self.dir), method, params),
            )
            .await
            .context("Background journal timed out")?
        } else {
            self.direct
                .as_ref()
                .context("Journal is locked")?
                .request(method, params)
                .await
        }
    }
    pub async fn capture_text(&self, text: &str) -> Result<Value> {
        self.request("capture_text", json!({"text":text})).await
    }
    pub async fn entries(&self, query: &str, trash: bool, limit: usize) -> Result<Vec<Value>> {
        if !trash && query.trim().is_empty() {
            let mut entries = Vec::new();
            while entries.len() < limit {
                let page = self
                    .request(
                        "feed",
                        json!({"offset":entries.len(), "limit":(limit - entries.len()).min(500)}),
                    )
                    .await?;
                let page = page["entries"].as_array().context("Missing entries")?;
                if page.is_empty() {
                    break;
                }
                entries.extend(page.iter().cloned());
                if page.len() < (limit - (entries.len() - page.len())).min(500) {
                    break;
                }
            }
            return Ok(entries);
        }
        let value = self
            .request(
                if trash { "trash" } else { "search" },
                json!({"query":memorious_core::mcp::fts_quote(query)}),
            )
            .await?;
        let entries = value["entries"].as_array().context("Missing entries")?;
        Ok(entries
            .iter()
            .filter(|e| {
                !trash
                    || query.trim().is_empty()
                    || e["text"]
                        .as_str()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&query.to_lowercase())
            })
            .take(limit)
            .cloned()
            .collect())
    }
    pub async fn redact(&self, id: &str) -> Result<()> {
        self.request("redact", json!({"event_id":id})).await?;
        Ok(())
    }
    pub async fn status(&self) -> Result<Value> {
        self.request("status", json!({})).await
    }
    pub async fn media(&self, hash: &str) -> Result<Vec<u8>> {
        let value = self.request("media_bytes", json!({"hash":hash})).await?;
        Ok(data_encoding::BASE64.decode(
            value["bytes_b64"]
                .as_str()
                .context("Missing media")?
                .as_bytes(),
        )?)
    }
    pub async fn capture_media(&self, kind: &str, bytes: Vec<u8>) -> Result<Value> {
        self.request("capture_media", json!({"kind":if kind == "voice" { "audio" } else { kind }, "bytes_b64":data_encoding::BASE64.encode(&bytes), "will_enrich":kind == "voice"})).await
    }
    pub async fn sync(&self, ticket: &str) -> Result<Value> {
        self.request("sync", json!({"ticket":ticket})).await
    }
    pub async fn enable_background(&mut self, executable: &Path) -> Result<()> {
        if self.background {
            return Ok(());
        }
        if self.password.is_empty() {
            bail!("Unlock this journal before enabling background mode");
        }
        if let Some(journal) = self.direct.take() {
            journal.close().await;
        }
        let result = async {
            let mut child = Command::new(executable)
                .arg(DAEMON_FLAG)
                .arg(&self.dir)
                .arg(socket(&self.dir))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            child
                .stdin
                .take()
                .context("Daemon stdin unavailable")?
                .write_all(self.password.as_bytes())?;
            for _ in 0..200 {
                if let Some(status) = child.try_wait()? {
                    bail!("Background journal exited: {status}");
                }
                if memorious_core::local::call(&socket(&self.dir), "health", json!({}))
                    .await
                    .is_ok()
                {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            let _ = child.kill();
            let _ = child.wait();
            bail!("Background journal did not start");
        }
        .await;
        match result {
            Ok(()) => {
                self.background = true;
                Ok(())
            }
            Err(error) => {
                self.direct = Some(DesktopJournal::open(&self.dir, &self.password).await?);
                Err(error)
            }
        }
    }
    pub async fn disable_background(&mut self, password: &str) -> Result<()> {
        if !self.background {
            return Ok(());
        }
        let password = if password.is_empty() {
            self.password.as_str()
        } else {
            password
        };
        if password.is_empty() {
            bail!("Enter the master password to return to foreground mode");
        }
        self.request("check_master_password", json!({"password":password}))
            .await?;
        let pid = self.request("health", json!({})).await?["pid"].as_u64();
        self.request("shutdown", json!({})).await?;
        for _ in 0..240 {
            if pid
                .map(|pid| unsafe { libc::kill(pid as i32, 0) == 0 })
                .unwrap_or(false)
            {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            if let Ok(journal) = DesktopJournal::open(&self.dir, password).await {
                self.direct = Some(journal);
                self.background = false;
                self.password = Zeroizing::new(password.into());
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        self.background = false;
        bail!("Background stopped; unlock the journal again");
    }
    pub async fn close(self) {
        if let Some(journal) = self.direct {
            journal.close().await;
        }
    }
}

struct Worker(Option<Child>);
impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            // Let the recorder finish and retain any pending note before exit.
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            for _ in 0..120 {
                if child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn start_worker(dir: &Path, socket: &Path) -> Result<Worker> {
    if !load_ptt(dir)?.enabled {
        return Ok(Worker(None));
    }
    let executable = std::env::current_exe()?.with_file_name("memorious-ptt");
    Ok(Worker(Some(
        Command::new(executable)
            .arg("--data-dir")
            .arg(dir)
            .arg("--socket")
            .arg(socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Start push-to-talk worker")?,
    )))
}
pub fn run_daemon() -> Option<Result<()>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).map(|a| a.as_os_str()) != Some(std::ffi::OsStr::new(DAEMON_FLAG)) {
        return None;
    }
    Some((|| {
        let dir = PathBuf::from(args.get(2).context("Missing journal directory")?);
        let socket = PathBuf::from(args.get(3).context("Missing socket")?);
        let mut password = Zeroizing::new(String::new());
        std::io::stdin().read_to_string(&mut password)?;
        tokio::runtime::Runtime::new()?.block_on(async {
            let daemon = memorious_core::local::Daemon::open(&dir, &password).await?;
            #[cfg(feature = "gui")]
            let _tray = crate::tray::spawn(&dir);
            let mut worker = match start_worker(&dir, &socket) {
                Ok(worker) => worker,
                Err(error) => {
                    std::fs::write(
                        dir.join(".memorious-ptt-status.json"),
                        json!({"state":"failed","error":error.to_string()}).to_string(),
                    )?;
                    Worker(None)
                }
            };
            let mut mark = Zeroizing::new(std::fs::read(dir.join(".memorious-ptt.json")).unwrap_or_default());
            let serve = daemon.serve(socket.clone());
            tokio::pin!(serve);
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
            let result = loop {
                tokio::select! {
                    result = &mut serve => break result,
                    _ = tick.tick() => {
                        let current = Zeroizing::new(std::fs::read(dir.join(".memorious-ptt.json")).unwrap_or_default());
                        if *current != *mark {
                            mark = current;
                            drop(worker);
                            let _ = std::fs::remove_file(control_socket(&dir));
                            let _ = std::fs::remove_file(dir.join(".memorious-ptt-status.json"));
                            worker = match start_worker(&dir, &socket) {
                                Ok(worker) => worker,
                                Err(error) => {
                                    let _ = std::fs::write(dir.join(".memorious-ptt-status.json"), json!({"state":"failed","error":error.to_string()}).to_string());
                                    Worker(None)
                                }
                            };
                        }
                    }
                }
            };
            drop(worker);
            let _ = std::fs::remove_file(dir.join(".memorious-ptt-status.json"));
            let _ = std::fs::remove_file(control_socket(&dir));
            result
        })
    })())
}
