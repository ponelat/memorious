//! Who may sync with this device. Holding the journal secret (a QR code anyone might photograph) only gets a peer to the door:
//! the first time an endpoint id shows up it is a *knock*, recorded and answered with nothing, until the owner taps accept on
//! this device. Removing a peer makes it a stranger again, so its next sync is a fresh knock.

use memorious_core::node::Node;
use memorious_core::trust::knock_code;
use memorious_core::Journal;
use tempfile::tempdir;

fn strict(path: &std::path::Path) -> Journal {
    let j = Journal::init(path, "pw").unwrap();
    j.set_require_approval(true).unwrap(); // the test build leaves the door open otherwise (feature `open-door`)
    j
}

#[test]
fn a_knock_code_is_short_stable_and_differs_per_endpoint() {
    let a = knock_code("a".repeat(64).as_str());
    assert_eq!(a, knock_code(&"a".repeat(64)));
    assert_ne!(a, knock_code(&"b".repeat(64)));
    assert_eq!(a.len(), 9, "XXXX-XXXX");
    assert_eq!(a.as_bytes()[4], b'-');
}

#[test]
fn a_stranger_is_not_allowed_until_accepted_and_a_removed_peer_is_a_stranger_again() {
    let d = tempdir().unwrap();
    let j = strict(&d.path().join("j"));
    assert!(!j.is_peer_allowed("peer-1").unwrap());
    j.record_knock("peer-1", Some("old ipad"), 1_000).unwrap();
    let k = j.knocks(2_000).unwrap();
    assert_eq!(k.len(), 1);
    assert_eq!((k[0].endpoint_id.as_str(), k[0].device.as_deref(), k[0].code.as_str()), ("peer-1", Some("old ipad"), knock_code("peer-1").as_str()));

    j.allow_peer("peer-1").unwrap();
    assert!(j.is_peer_allowed("peer-1").unwrap());
    assert!(j.knocks(2_000).unwrap().is_empty(), "accepting answers the knock");

    // it was a known peer with all its bookkeeping; removing forgets all of it
    j.record_sync_contact("peer-1", 5_000, Some("dev-x"), "ticket").unwrap();
    j.set_peer_required("peer-1", false).unwrap();
    j.remove_peer("peer-1").unwrap();
    assert!(!j.is_peer_allowed("peer-1").unwrap());
    assert!(j.store.meta_scan("peer_").unwrap().iter().all(|(k, _)| !k.ends_with("peer-1")), "no trace of the peer is left");
}

#[test]
fn knocks_expire_after_a_day_repeat_knocks_collapse_and_the_list_is_capped() {
    let d = tempdir().unwrap();
    let j = strict(&d.path().join("j"));
    j.record_knock("p", None, 0).unwrap();
    j.record_knock("p", Some("name"), 500).unwrap();
    let k = j.knocks(600).unwrap();
    assert_eq!((k.len(), k[0].first_ms, k[0].last_ms, k[0].device.as_deref()), (1, 0, 500, Some("name")));
    assert!(j.knocks(25 * 3_600_000).unwrap().is_empty(), "a day later it is forgotten");
    for i in 0..40 {
        j.record_knock(&format!("spam-{i}"), None, 10_000 + i).unwrap();
    }
    assert_eq!(j.knocks(11_000).unwrap().len(), 20, "a flood cannot grow the list without bound");
    j.reject_knock("spam-39").unwrap();
    assert!(!j.knocks(11_000).unwrap().iter().any(|k| k.endpoint_id == "spam-39"));
}

#[test]
fn peers_known_before_this_feature_are_let_in_once() {
    let d = tempdir().unwrap();
    let j = strict(&d.path().join("j"));
    j.record_sync_contact("old-peer", 1, None, "ticket").unwrap();
    assert!(!j.is_peer_allowed("old-peer").unwrap());
    j.grandfather_known_peers().unwrap();
    assert!(j.is_peer_allowed("old-peer").unwrap());
    j.remove_peer("old-peer").unwrap();
    j.record_sync_contact("old-peer", 2, None, "ticket").unwrap();
    j.grandfather_known_peers().unwrap();
    assert!(!j.is_peer_allowed("old-peer").unwrap(), "it ran once: a removed peer is not let back in");
}

#[tokio::test]
async fn a_peer_with_the_secret_gets_nothing_until_the_owner_accepts_and_removal_resets_it() {
    let dir = tempdir().unwrap();
    let ja = Journal::init(&dir.path().join("a"), "pw").unwrap();
    ja.capture_text("from the door keeper").unwrap();
    ja.set_require_approval(true).unwrap();
    let jb = Journal::init_with_secret(&dir.path().join("b"), *ja.secret(), "pw").unwrap();
    let a = Node::spawn(ja).await.unwrap();
    let b = Node::spawn(jb).await.unwrap();

    // B knocks: refused, and told why; A learns B's id and code
    let err = b.sync_events_with(&a.addr()).await.unwrap_err();
    assert!(format!("{err:#}").contains("waiting for approval"), "{err:#}");
    assert_eq!(b.journal().list().unwrap().len(), 0, "nothing came back through the door");
    let knocks = a.journal().knocks(memorious_core::store::now_ms()).unwrap();
    assert_eq!(knocks.len(), 1);
    assert_eq!(knocks[0].endpoint_id, b.endpoint_id().to_string());
    assert_eq!(knocks[0].code, knock_code(&b.endpoint_id().to_string()));

    // A taps accept
    a.journal().allow_peer(&knocks[0].endpoint_id).unwrap();
    b.sync_events_with(&a.addr()).await.unwrap();
    assert_eq!(b.journal().list().unwrap().len(), 1);

    // A removes B: the next sync is a fresh knock, with nothing sent
    a.journal().remove_peer(&b.endpoint_id().to_string()).unwrap();
    a.journal().capture_text("after the removal").unwrap();
    let err = b.sync_events_with(&a.addr()).await.unwrap_err();
    assert!(format!("{err:#}").contains("waiting for approval"), "{err:#}");
    assert_eq!(b.journal().list().unwrap().len(), 1, "the new entry did not leak");
    assert_eq!(a.journal().knocks(memorious_core::store::now_ms()).unwrap().len(), 1);
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn a_joining_device_waits_showing_its_code_until_the_owner_accepts() {
    let dir = tempdir().unwrap();
    let ja = strict(&dir.path().join("a"));
    ja.capture_text("hello").unwrap();
    let a = std::sync::Arc::new(Node::spawn(ja).await.unwrap());
    let ticket = a.ticket().unwrap();

    // the owner taps accept a little after the knock shows up, once the code is on the joiner's screen
    let owner = a.clone();
    let tapper = tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if let (Some(p), Some(k)) = (memorious_core::node::join_progress(), owner.journal().knocks(memorious_core::store::now_ms()).unwrap().first().cloned()) {
                assert_eq!(p.code, k.code, "the joiner shows the code the approver sees");
                owner.accept_knock(&k.endpoint_id).unwrap();
                return;
            }
        }
    });
    let (b, _) = Node::pair_from_ticket(&dir.path().join("b"), &ticket, "pw").await.unwrap();
    tapper.await.unwrap();
    assert_eq!(b.journal().list().unwrap().len(), 1);
    assert!(memorious_core::node::join_progress().is_none(), "nothing is pending any more");
    b.shutdown().await;
}
