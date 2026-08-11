//! CLI harness for the core: the M1 proving ground.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use memorious_core::event::{MediaKind, Payload};
use memorious_core::node::JournalTicket;
use memorious_core::{Journal, Node};

#[derive(Parser)]
#[command(name = "journal", about = "Infinite Journal peer")]
struct Cli {
    /// Journal data directory.
    #[arg(long, global = true, default_value_os_t = default_data_dir())]
    data: PathBuf,
    /// Master password (or set MEMORIOUS_PASSWORD; prompts interactively otherwise).
    #[arg(long, global = true)]
    password: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

/// Flag → env → interactive prompt. `confirm` double-prompts (init/migrate,
/// where a typo would seal data behind a password nobody knows).
fn resolve_password(flag: &Option<String>, confirm: bool) -> Result<String> {
    if let Some(p) = flag {
        return Ok(p.clone());
    }
    if let Ok(p) = std::env::var("MEMORIOUS_PASSWORD") {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    let p = rpassword::prompt_password("master password: ").context("read password")?;
    if p.is_empty() {
        bail!("empty password");
    }
    if confirm {
        let again = rpassword::prompt_password("repeat password: ")?;
        if p != again {
            bail!("passwords don't match");
        }
    }
    Ok(p)
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("MEMORIOUS_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs_home()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".memorious")
        })
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a fresh journal.
    Init,
    /// Append a text entry.
    Add { text: String },
    /// Append a photo from a file.
    AddPhoto { file: PathBuf },
    /// Append an audio note from a file (m4a).
    AddAudio { file: PathBuf },
    /// Show the timeline (newest last).
    List,
    /// Move an entry to trash.
    Redact { event_id: String },
    /// Show trashed entries.
    Trash,
    /// Full-text search.
    Search { query: String },
    /// Set the browser passcode.
    SetPasscode { passcode: String },
    /// Run as a peer until Ctrl-C, printing a pairing ticket.
    Serve,
    /// Print a pairing ticket and keep serving until Ctrl-C.
    Ticket,
    /// Create a new journal from a pairing ticket and pull everything.
    Join { ticket: String },
    /// Sync once with the peer in the ticket.
    Sync { ticket: String },
    /// Per-device heads and journal facts.
    Status,
    /// Import a v1 export JSON (photos fetched from --base via curl).
    ImportV1 {
        file: PathBuf,
        /// Base URL for relative photo_url values, e.g. https://v1 prod
        #[arg(long, default_value = "https://v1 prod")]
        base: String,
    },
    /// Write the derived year/month/day markdown tree + media files.
    ExportMd { out: PathBuf },
    /// Encrypt a pre-encryption journal in place (old dir kept as backup).
    MigrateEncrypt,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let cli = Cli::parse();
    let data = cli.data;
    let pw = cli.password;
    let open = |pw: &Option<String>| -> Result<Journal> {
        Journal::open(&data, &resolve_password(pw, false)?)
    };

    match cli.cmd {
        Cmd::Init => {
            let password = resolve_password(&pw, true)?;
            let j = Journal::init(&data, &password)?;
            println!("journal created at {}", data.display());
            println!("device: {}", j.device_id());
        }
        Cmd::Add { text } => {
            let j = open(&pw)?;
            let e = j.capture_text(&text)?;
            println!("{}", e.event_id);
        }
        Cmd::AddPhoto { file } => add_media(&data, &pw, MediaKind::Photo, &file).await?,
        Cmd::AddAudio { file } => add_media(&data, &pw, MediaKind::Audio, &file).await?,
        Cmd::List => {
            let j = open(&pw)?;
            for e in j.list()? {
                println!("{}", format_entry(&e));
            }
        }
        Cmd::Redact { event_id } => {
            let j = open(&pw)?;
            j.redact(&event_id)?;
            println!("redacted {event_id}");
        }
        Cmd::Trash => {
            let j = open(&pw)?;
            for e in j.trash()? {
                println!("{}", format_entry(&e));
            }
        }
        Cmd::Search { query } => {
            let j = open(&pw)?;
            let redacted = j.store.redacted_ids()?;
            for id in j.store.search(&query)? {
                if let Some(e) = j.store.get_event(&id)? {
                    if !redacted.contains(&e.event_id) {
                        println!("{}", format_entry(&e));
                    }
                }
            }
        }
        Cmd::SetPasscode { passcode } => {
            let j = open(&pw)?;
            j.set_passcode(&passcode)?;
            println!("passcode set (syncs to all peers)");
        }
        Cmd::Serve | Cmd::Ticket => {
            let node = Node::spawn(open(&pw)?).await?;
            node.dialable_addr().await?;
            println!("device: {}", node.journal().device_id());
            println!("ticket: {}", node.ticket()?);
            println!("serving — Ctrl-C to stop");
            tokio::signal::ctrl_c().await?;
            node.shutdown().await;
        }
        Cmd::Join { ticket } => {
            if data.join("db.sqlite").exists() {
                bail!("journal already exists at {} — use `sync`", data.display());
            }
            let password = resolve_password(&pw, true)?;
            let (node, report) = Node::join_from_ticket(&data, &ticket, &password).await?;
            println!(
                "joined: received {} events, {} blobs",
                report.received, report.blobs_fetched
            );
            node.shutdown().await;
        }
        Cmd::Sync { ticket } => {
            let node = Node::spawn(open(&pw)?).await?;
            let t = JournalTicket::decode(&ticket)?;
            if &t.secret != node.journal().secret() {
                bail!("ticket is for a different journal");
            }
            let report = node.sync_with(&t.addr()?).await?;
            println!(
                "synced: sent {}, received {}, blobs fetched {}",
                report.sent, report.received, report.blobs_fetched
            );
            node.shutdown().await;
        }
        Cmd::ImportV1 { file, base } => {
            let export: memorious_core::import_v1::V1Export =
                serde_json::from_slice(&std::fs::read(&file)?).context("parse export json")?;
            let node = Node::spawn(open(&pw)?).await?;
            let report = memorious_core::import_v1::import_v1(&node, &export, |url| {
                let full = if url.starts_with("http") {
                    url.to_string()
                } else {
                    format!("{}{}", base.trim_end_matches('/'), url)
                };
                let out = std::process::Command::new("curl")
                    .args(["-sf", "--max-time", "60", &full])
                    .output()?;
                if !out.status.success() {
                    anyhow::bail!("curl failed for {full}");
                }
                Ok(out.stdout)
            })
            .await?;
            println!(
                "imported: {} text, {} photos; skipped {} (already imported); {} photo failures",
                report.text_entries, report.photo_entries, report.skipped, report.photo_failures
            );
            node.shutdown().await;
        }
        Cmd::ExportMd { out } => {
            let node = Node::spawn(open(&pw)?).await?;
            let report = memorious_core::export_md::export_markdown(&node, &out).await?;
            println!(
                "export: {} day files written, {} unchanged; {} media written, {} unchanged",
                report.day_files_written,
                report.day_files_unchanged,
                report.media_written,
                report.media_unchanged
            );
            node.shutdown().await;
        }
        Cmd::MigrateEncrypt => {
            let password = resolve_password(&pw, true)?;
            let report = memorious_core::migrate::migrate_encrypt(&data, &password).await?;
            println!(
                "encrypted: {} events, {} media blobs ({} bytes) re-encrypted",
                report.events, report.media, report.media_bytes
            );
            println!("old journal kept at {}", report.backup_dir.display());
            println!("other devices must re-pair (delete their data dir, then `join`)");
        }
        Cmd::Status => {
            let j = open(&pw)?;
            println!("journal: {}", data.display());
            println!("device: {}", j.device_id());
            println!("entries: {}", j.list()?.len());
            println!("trash: {}", j.trash()?.len());
            println!("heads:");
            for (device, seq) in j.store.heads()? {
                let marker = if device == j.device_id() { " (this device)" } else { "" };
                println!("  {device} @ {seq}{marker}");
            }
        }
    }
    Ok(())
}

async fn add_media(
    data: &std::path::Path,
    pw: &Option<String>,
    kind: MediaKind,
    file: &std::path::Path,
) -> Result<()> {
    let bytes = std::fs::read(file).with_context(|| format!("read {}", file.display()))?;
    let node = Node::spawn(Journal::open(data, &resolve_password(pw, false)?)?).await?;
    let e = node.capture_blob(kind, bytes).await?;
    println!("{}", e.event_id);
    node.shutdown().await;
    Ok(())
}

fn format_entry(e: &memorious_core::Event) -> String {
    let ts = chrono_like(e.recorded_at);
    let body = match &e.payload {
        Payload::Text { text } => text.replace('\n', " ⏎ "),
        Payload::Photo { hash, size, .. } => format!("[photo {} {}B]", &hash[..8.min(hash.len())], size),
        Payload::Audio { hash, size, .. } => format!("[audio {} {}B]", &hash[..8.min(hash.len())], size),
        other => format!("{other:?}"),
    };
    format!("{ts}  {}  {body}", e.event_id)
}

/// Tiny UTC formatter (no chrono dependency): "YYYY-MM-DD HH:MM".
fn chrono_like(ms: i64) -> String {
    let secs = ms / 1000;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, m) = (tod / 3600, (tod % 3600) / 60);
    // civil-from-days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}
