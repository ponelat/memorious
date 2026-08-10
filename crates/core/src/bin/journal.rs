//! CLI harness for the core: the M1 proving ground.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use journal_core::event::{MediaKind, Payload};
use journal_core::node::JournalTicket;
use journal_core::{Journal, Node};

#[derive(Parser)]
#[command(name = "journal", about = "Infinite Journal peer")]
struct Cli {
    /// Journal data directory.
    #[arg(long, global = true, default_value_os_t = default_data_dir())]
    data: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

fn default_data_dir() -> PathBuf {
    std::env::var_os("JOURNAL_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs_home()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".infinite-journal")
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

    match cli.cmd {
        Cmd::Init => {
            let j = Journal::init(&data)?;
            println!("journal created at {}", data.display());
            println!("device: {}", j.device_id());
        }
        Cmd::Add { text } => {
            let j = Journal::open(&data)?;
            let e = j.capture_text(&text)?;
            println!("{}", e.event_id);
        }
        Cmd::AddPhoto { file } => add_media(&data, MediaKind::Photo, &file).await?,
        Cmd::AddAudio { file } => add_media(&data, MediaKind::Audio, &file).await?,
        Cmd::List => {
            let j = Journal::open(&data)?;
            for e in j.list()? {
                println!("{}", format_entry(&e));
            }
        }
        Cmd::Redact { event_id } => {
            let j = Journal::open(&data)?;
            j.redact(&event_id)?;
            println!("redacted {event_id}");
        }
        Cmd::Trash => {
            let j = Journal::open(&data)?;
            for e in j.trash()? {
                println!("{}", format_entry(&e));
            }
        }
        Cmd::Search { query } => {
            let j = Journal::open(&data)?;
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
            let j = Journal::open(&data)?;
            j.set_passcode(&passcode)?;
            println!("passcode set (syncs to all peers)");
        }
        Cmd::Serve | Cmd::Ticket => {
            let node = Node::spawn(Journal::open(&data)?).await?;
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
            let (node, report) = Node::join_from_ticket(&data, &ticket).await?;
            println!(
                "joined: received {} events, {} blobs",
                report.received, report.blobs_fetched
            );
            node.shutdown().await;
        }
        Cmd::Sync { ticket } => {
            let node = Node::spawn(Journal::open(&data)?).await?;
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
        Cmd::Status => {
            let j = Journal::open(&data)?;
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

async fn add_media(data: &std::path::Path, kind: MediaKind, file: &std::path::Path) -> Result<()> {
    let bytes = std::fs::read(file).with_context(|| format!("read {}", file.display()))?;
    let node = Node::spawn(Journal::open(data)?).await?;
    let e = node.capture_blob(kind, bytes).await?;
    println!("{}", e.event_id);
    node.shutdown().await;
    Ok(())
}

fn format_entry(e: &journal_core::Event) -> String {
    let ts = chrono_like(e.recorded_at);
    let body = match &e.payload {
        Payload::Text { text } => text.replace('\n', " ⏎ "),
        Payload::Photo { hash, size } => format!("[photo {} {}B]", &hash[..8.min(hash.len())], size),
        Payload::Audio { hash, size } => format!("[audio {} {}B]", &hash[..8.min(hash.len())], size),
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
