//! M1 acceptance: two peers, offline captures, sync → identical timelines, media included.

use memorious_core::event::MediaKind;
use memorious_core::node::Node;
use memorious_core::Journal;
use tempfile::tempdir;

fn timeline_ids(j: &Journal) -> Vec<String> {
    j.list()
        .unwrap()
        .iter()
        .map(|e| e.event_id.clone())
        .collect()
}

/// Does any file under `root` contain `needle`? (Encryption-at-rest check.)
fn dir_contains_bytes(root: &std::path::Path, needle: &[u8]) -> bool {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                if bytes.windows(needle.len()).any(|w| w == needle) {
                    return true;
                }
            }
        }
    }
    false
}

#[tokio::test]
async fn two_peers_converge_including_media() {
    let dir = tempdir().unwrap();

    // Journal born on A; B joins with A's secret (as ticket redemption would).
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();

    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();

    // Captures made "offline of each other".
    a.journal().capture_text("from a: one").unwrap();
    a.journal().capture_text("from a: two").unwrap();
    b.journal().capture_text("from b: one").unwrap();
    let photo_bytes = b"not really a jpeg but bytes all the same".to_vec();
    let photo = a
        .capture_blob(memorious_core::event::MediaKind::Photo, photo_bytes.clone())
        .await
        .unwrap();

    // One round-trip sync started from B.
    b.sync_with(&a.addr()).await.unwrap();

    assert_eq!(timeline_ids(a.journal()), timeline_ids(b.journal()));
    assert_eq!(a.journal().list().unwrap().len(), 4);

    // Media arrived on B, byte-identical after decryption.
    let hash = photo.blob_hash().unwrap();
    let got = b.blob_bytes(hash).await.unwrap();
    assert_eq!(got, photo_bytes);

    // …but at rest, neither peer's disk holds the plaintext anywhere: not in
    // the blob store (ciphertext identities) and not in the database file.
    for root in [dir.path().join("a"), dir.path().join("b")] {
        assert!(
            !dir_contains_bytes(&root, &photo_bytes),
            "plaintext media found on disk under {}",
            root.display()
        );
    }

    // Redaction made after sync propagates on the next sync.
    let toss = b.journal().capture_text("delete me").unwrap();
    b.journal().redact(&toss.event_id).unwrap();
    b.sync_with(&a.addr()).await.unwrap();
    assert_eq!(timeline_ids(a.journal()), timeline_ids(b.journal()));

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn wrong_secret_is_rejected() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init(&dir.path().join("b"), "pw").unwrap(); // different journal, own secret

    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();

    a.journal().capture_text("private").unwrap();
    assert!(b.sync_with(&a.addr()).await.is_err());
    assert_eq!(b.journal().list().unwrap().len(), 0);

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn interrupted_sync_converges_on_retry() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let _jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();

    let a = Node::spawn(ja).await.unwrap();
    for i in 0..50 {
        a.journal().capture_text(&format!("entry {i}")).unwrap();
    }

    // First attempt dies mid-flight: B connects, then is dropped hard.
    {
        let b = Node::spawn(Journal::open(&dir.path().join("b"), "pw").unwrap())
            .await
            .unwrap();
        let addr = a.addr();
        let handle = tokio::spawn(async move { b.sync_with(&addr).await.map(|_| ()) });
        // Let it get partway, then abort the task (connection torn down mid-protocol).
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        handle.abort();
        let _ = handle.await;
    }

    // Retry with a fresh node over the same data dir must converge, not error.
    let b = Node::spawn(Journal::open(&dir.path().join("b"), "pw").unwrap())
        .await
        .unwrap();
    b.sync_with(&a.addr()).await.unwrap();
    assert_eq!(timeline_ids(a.journal()), timeline_ids(b.journal()));
    assert_eq!(b.journal().list().unwrap().len(), 50);

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn sync_health_traffic_light() {
    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let secret = *ja.secret();
    let a = Node::spawn(ja).await.unwrap();
    a.journal().capture_text("solo entry").unwrap();

    // A journal with no known peers has nowhere to push — green.
    assert_eq!(a.journal().sync_health(now_ms()).unwrap().color, "green");

    let jb = Journal::init_with_secret(&dir.path().join("b"), secret, "pw").unwrap();
    let b = Node::spawn(jb).await.unwrap();
    b.sync_with(&a.addr()).await.unwrap();

    // Converged, fresh contact on both sides (dialer and acceptor).
    assert_eq!(a.journal().sync_health(now_ms()).unwrap().color, "green");
    assert_eq!(b.journal().sync_health(now_ms()).unwrap().color, "green");

    // New local data the peer hasn't seen — yellow.
    a.journal().capture_text("unsent").unwrap();
    assert_eq!(a.journal().sync_health(now_ms()).unwrap().color, "yellow");
    a.sync_with(&b.addr()).await.unwrap();
    assert_eq!(a.journal().sync_health(now_ms()).unwrap().color, "green");

    // No contact for 48h — red, and red outranks pending.
    let later = now_ms() + 49 * 3600 * 1000;
    assert_eq!(a.journal().sync_health(later).unwrap().color, "red");

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn video_capture_round_trips() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();

    let bytes = b"definitely an mp4".to_vec();
    let ev = a
        .capture_blob(memorious_core::event::MediaKind::Video, bytes.clone())
        .await
        .unwrap();
    let json = memorious_core::api_json::entry_json(&ev);
    assert_eq!(json["kind"], "video");
    assert!(json["media"]["hash"].is_string());

    b.sync_with(&a.addr()).await.unwrap();
    assert_eq!(timeline_ids(a.journal()), timeline_ids(b.journal()));
    let got = b.blob_bytes(ev.blob_hash().unwrap()).await.unwrap();
    assert_eq!(got, bytes);

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn pairing_defers_media_to_background_sync() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    a.journal().capture_text("hello new device").unwrap();
    let photo_bytes = b"heavy pixels".to_vec();
    let photo = a
        .capture_blob(memorious_core::event::MediaKind::Photo, photo_bytes.clone())
        .await
        .unwrap();

    // Pairing converges the event log but leaves media for a later sync, so a
    // device joining a media-heavy journal is usable immediately.
    let ticket = a.ticket().unwrap();
    let (b, report) = Node::pair_from_ticket(&dir.path().join("b"), &ticket, "pw")
        .await
        .unwrap();
    assert_eq!(timeline_ids(a.journal()), timeline_ids(b.journal()));
    assert_eq!(report.blobs_fetched, 0);
    let hash = photo.blob_hash().unwrap();
    assert!(!b.has_blob(hash).await.unwrap());

    // The wrong master password is still caught at pair time — the proof
    // unwraps a media key from the event log, no blob bytes needed.
    let err = Node::pair_from_ticket(&dir.path().join("c"), &a.ticket().unwrap(), "wrong")
        .await
        .err()
        .expect("wrong password must be rejected at pair time");
    assert!(
        format!("{err:#}").contains("master password"),
        "unexpected error: {err:#}"
    );

    // An ordinary follow-up sync brings the media across.
    let report = b.sync_with(&a.addr()).await.unwrap();
    assert_eq!(report.blobs_fetched, 1);
    assert_eq!(b.blob_bytes(hash).await.unwrap(), photo_bytes);

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn peers_learn_device_ids_names_and_origin() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    ja.ensure_device_name("web").unwrap();
    jb.ensure_device_name("iPhone").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();
    a.journal().capture_text("an entry so the timeline has a span").unwrap();

    b.sync_with(&a.addr()).await.unwrap();

    // Each side knows the other's device id, and how the peer was discovered:
    // B redeemed a ticket for A; A learned B when it connected in.
    let a_peers = a.peers().await.unwrap();
    let b_peers = b.peers().await.unwrap();
    assert_eq!(a_peers.len(), 1);
    assert_eq!(b_peers.len(), 1);
    assert_eq!(a_peers[0].device_id.as_deref(), Some(b.journal().device_id()));
    assert_eq!(b_peers[0].device_id.as_deref(), Some(a.journal().device_id()));
    assert_eq!(a_peers[0].discovery.as_deref(), Some("inbound"));
    assert_eq!(b_peers[0].discovery.as_deref(), Some("ticket"));
    assert!(a_peers[0].last_ok_ms > 0);
    assert_eq!(a_peers[0].endpoint_id, b.endpoint().id().to_string());

    // Right after a loopback sync the transport is a direct LAN path with no
    // proxy (relay) in the data chain.
    let conn = b_peers[0].conn.as_ref().expect("fresh contact has a live path");
    assert_eq!(conn.transport, "direct");
    assert!(conn.lan, "loopback must classify as LAN: {conn:?}");
    assert!(!conn.proxied, "direct path must report no proxy: {conn:?}");

    // Names went along with the event log (they are annotation events).
    let names = b.journal().device_names().unwrap();
    assert_eq!(names.get(a.journal().device_id()).map(String::as_str), Some("web"));
    assert_eq!(names.get(b.journal().device_id()).map(String::as_str), Some("iPhone"));

    // The shared status JSON carries everything the status screens need.
    let v = a.status_json().await.unwrap();
    assert_eq!(v["device_id"], a.journal().device_id());
    assert!(v["storage"]["db_bytes"].as_u64().unwrap() > 0);
    assert!(v["timeline"]["first_recorded_at"].is_i64());
    assert!(v["timeline"]["last_recorded_at"].is_i64());
    assert_eq!(v["names"][b.journal().device_id()], "iPhone");
    assert_eq!(v["peers"][0]["device_id"], b.journal().device_id());
    assert_eq!(v["health"]["color"], "green");
    assert_eq!(v["net"]["relay_mode"], "default");

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn peer_joins_propagate_to_every_peer() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    ja.ensure_peer_join().unwrap();
    jb.ensure_peer_join().unwrap();
    let a_id = ja.device_id().to_string();
    let b_id = jb.device_id().to_string();
    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();

    b.sync_with(&a.addr()).await.unwrap();

    // Each device's join is a real event: it propagates to the other peer,
    // just like a capture or a device name — no local-only "known peers"
    // list involved.
    let a_joined: Vec<String> =
        a.journal().peer_joins().unwrap().into_iter().map(|(id, _)| id).collect();
    let b_joined: Vec<String> =
        b.journal().peer_joins().unwrap().into_iter().map(|(id, _)| id).collect();
    assert!(a_joined.contains(&a_id) && a_joined.contains(&b_id), "a should know both joins: {a_joined:?}");
    assert!(b_joined.contains(&a_id) && b_joined.contains(&b_id), "b should know both joins: {b_joined:?}");

    // Every app launch calls this again; it must never double-write.
    a.journal().ensure_peer_join().unwrap();
    let a_own_joins = a.journal().peer_joins().unwrap().into_iter().filter(|(id, _)| id == &a_id).count();
    assert_eq!(a_own_joins, 1, "ensure_peer_join must be idempotent");

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn version_announce_republishes_only_when_the_version_changes() {
    let dir = tempdir().unwrap();
    let j = Journal::init(&dir.path().join("a"), "pw").unwrap();

    // Every launch calls this; a build that hasn't changed must not spam
    // the log (same shape as ensure_peer_join's idempotency).
    j.ensure_version_seen().unwrap();
    j.ensure_version_seen().unwrap();
    let mine: Vec<_> = j
        .peer_versions()
        .unwrap()
        .into_iter()
        .filter(|(id, _)| id == j.device_id())
        .collect();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].1, memorious_core::VERSION);

    // Simulate an older build's leftover announcement, then a relaunch on
    // this (current) build — unlike peer_join, this must write again,
    // since the fact ("what am I running") has changed.
    j.append_infra("version_seen", Some(j.device_id()), serde_json::json!({ "version": "0.0.1" }))
        .unwrap();
    j.ensure_version_seen().unwrap();
    let latest = j.peer_versions().unwrap();
    assert_eq!(latest.get(j.device_id()).map(String::as_str), Some(memorious_core::VERSION));
}

#[tokio::test]
async fn newer_version_available_reports_the_max_peer_version() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    ja.ensure_version_seen().unwrap();
    // b announces a build newer than anything this test binary could ever
    // actually be — standing in for "b has already updated, a hasn't yet".
    jb.append_infra("version_seen", Some(jb.device_id()), serde_json::json!({ "version": "99.0.0" }))
        .unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();

    b.sync_with(&a.addr()).await.unwrap();

    assert_eq!(a.journal().newer_version_available().unwrap().as_deref(), Some("99.0.0"));
    assert_eq!(
        a.journal().peer_versions().unwrap().get(b.journal().device_id()).map(String::as_str),
        Some("99.0.0")
    );

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn master_password_rotation_keeps_old_media_readable_for_every_peer() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "old-pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "old-pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();

    let photo = a.capture_blob(MediaKind::Photo, b"a photo from before the rotation".to_vec()).await.unwrap();
    let hash = photo.blob_hash().unwrap().to_string();

    // B converges fully (events + the actual blob) before the rotation.
    b.sync_with(&a.addr()).await.unwrap();
    assert_eq!(b.blob_bytes(&hash).await.unwrap(), b"a photo from before the rotation");

    // A rotates. It must still read its own pre-rotation photo immediately.
    a.journal().rotate_master_password("new-pw").unwrap();
    assert_eq!(a.blob_bytes(&hash).await.unwrap(), b"a photo from before the rotation");

    // The rewrap (and the moved password proof) sync to B like any other
    // event — but B hasn't adopted the new password yet. It must still read
    // the same old photo exactly as before: rotating on A must never break
    // a peer that hasn't caught up.
    b.sync_with(&a.addr()).await.unwrap();
    assert_eq!(
        b.blob_bytes(&hash).await.unwrap(),
        b"a photo from before the rotation",
        "a peer that hasn't adopted the rotation must keep reading old media with its old key"
    );

    // Adopting with the WRONG password is rejected, and changes nothing.
    assert!(b.journal().adopt_master_password("not-it").is_err());
    assert_eq!(b.blob_bytes(&hash).await.unwrap(), b"a photo from before the rotation");

    // Once B adopts the real new password, it reads the very same photo —
    // now via the rewrap instead of the original envelope — and a fresh
    // capture made after adopting round-trips too.
    b.journal().adopt_master_password("new-pw").unwrap();
    assert_eq!(b.blob_bytes(&hash).await.unwrap(), b"a photo from before the rotation");
    let new_photo = b.capture_blob(MediaKind::Photo, b"captured after adopting".to_vec()).await.unwrap();
    assert_eq!(
        b.blob_bytes(new_photo.blob_hash().unwrap()).await.unwrap(),
        b"captured after adopting"
    );

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn relays_disabled_peers_still_sync_on_lan() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();

    // LAN-only network config: no relays, no public address lookup.
    let cfg = memorious_core::node::NetConfig {
        relay_mode: "disabled".into(),
        relay_urls: vec![],
        public_lookup: false,
    };
    ja.set_net_config(&cfg).unwrap();
    jb.set_net_config(&cfg).unwrap();
    assert_eq!(ja.net_config(), cfg);

    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();
    a.journal().capture_text("over the LAN only").unwrap();
    b.sync_with(&a.addr()).await.unwrap();
    assert_eq!(timeline_ids(a.journal()), timeline_ids(b.journal()));

    // The ticket still works — it carries direct addresses.
    assert!(a.ticket().unwrap().starts_with("memorious"));

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn ticket_pairing_round_trip() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    a.journal().capture_text("hello new device").unwrap();

    // Ticket string carries secret + address; a fresh device joins from it alone.
    let ticket = a.ticket().unwrap();
    let (b, report) = Node::join_from_ticket(&dir.path().join("b"), &ticket, "pw")
        .await
        .unwrap();
    assert!(report.received > 0);
    assert_eq!(timeline_ids(a.journal()), timeline_ids(b.journal()));

    // A ticket alone is not enough anymore: joining with the wrong master
    // password is caught as soon as a media key fails to unwrap.
    let photo = a
        .capture_blob(memorious_core::event::MediaKind::Photo, b"pixels".to_vec())
        .await
        .unwrap();
    let _ = photo;
    let err = Node::join_from_ticket(&dir.path().join("c"), &a.ticket().unwrap(), "wrong")
        .await
        .err()
        .expect("wrong password must be rejected");
    assert!(
        format!("{err:#}").contains("master password"),
        "unexpected error: {err:#}"
    );
    // A failed join leaves nothing behind, so the same directory can be
    // retried — the iPad bug of 2026-09-12: the first attempt created
    // db.sqlite, every later tap of "join" said "journal already exists".
    assert!(
        !dir.path().join("c").join("db.sqlite").exists(),
        "failed join must not leave a journal behind"
    );
    let (c, _) = Node::join_from_ticket(&dir.path().join("c"), &a.ticket().unwrap(), "pw")
        .await
        .expect("retry after a failed join must work");
    assert_eq!(timeline_ids(a.journal()), timeline_ids(c.journal()));

    a.shutdown().await;
    b.shutdown().await;
    c.shutdown().await;
}

#[tokio::test]
async fn unreachable_peer_join_leaves_no_journal() {
    let dir = tempdir().unwrap();
    // A real ticket whose peer is gone: mint it, then shut the peer down.
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let ticket = a.ticket().unwrap();
    a.shutdown().await;

    let root = dir.path().join("b");
    let err = Node::pair_from_ticket(&root, &ticket, "pw").await.err();
    assert!(err.is_some(), "joining a dead peer must fail");
    assert!(!root.join("db.sqlite").exists(), "no db.sqlite after a failed join");
    assert!(!root.join("keys.json").exists(), "no keys.json after a failed join");
    assert!(!root.join("blobs").exists(), "no blob store after a failed join");
}

#[tokio::test]
async fn text_only_journal_rejects_wrong_password_on_join() {
    // 2026-09-12: a rubbish master password "got through" on an iPad because
    // the journal it joined had no media yet — unwrapping a media key was the
    // only password check. The creator now publishes a proof at init.
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    a.journal().capture_text("text only, no media").unwrap();
    let ticket = a.ticket().unwrap();

    let err = Node::pair_from_ticket(&dir.path().join("b"), &ticket, "rubbish")
        .await
        .err()
        .expect("wrong password must be rejected even with no media");
    assert!(format!("{err:#}").contains("master password"), "unexpected: {err:#}");
    assert!(!dir.path().join("b").join("db.sqlite").exists());

    let (b, _) = Node::pair_from_ticket(&dir.path().join("b"), &ticket, "pw")
        .await
        .expect("right password joins");
    assert_eq!(timeline_ids(a.journal()), timeline_ids(b.journal()));
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn legacy_journal_without_proof_gets_one_from_its_creator() {
    use memorious_core::journal::PASSWORD_PROOF_TARGET;
    let dir = tempdir().unwrap();
    // A journal made before proofs existed: init_with_secret writes none.
    let secret = [7u8; 32];
    let root = dir.path().join("legacy");
    {
        let j = Journal::init_with_secret(&root, secret, "pw").unwrap();
        j.capture_text("old text").unwrap();
        assert!(j.password_proof().unwrap().is_none());
    }
    // The creator's next open publishes it; a wrong password then fails to open.
    let j = Journal::open(&root, "pw").unwrap();
    assert!(j.password_proof().unwrap().is_some());
    assert!(j.annotations().unwrap().contains_key(PASSWORD_PROOF_TARGET));
    drop(j);
    // The DB itself is keyed from the password, so a wrong one never opens;
    // the proof matters for joiners and for devices that joined wrongly.
    assert!(Journal::open(&root, "nope").is_err());
}
