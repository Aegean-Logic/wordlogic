//! The desktop's Nextcloud end to end, against a fake Nextcloud on a local port
//! ([`super::fake_server`]): listing and opening files, sending saves back with ETags, conflicts,
//! Save As, and co-editing a document with someone in the browser (a second engine session
//! standing in for the browser's WordLogic) through the app's sequencer, state snapshots and
//! checkpoints included.

use std::time::{Duration, Instant};

use serde_json::json;
use wordcraft_engine::Session;
use wordcraft_engine::collab::{Incoming, PROTOCOL};
use wordcraft_engine::doc::{Document, StoryRef};
use wordcraft_ui_egui::{Services, WordApp};

use super::fake_server::{Server, USER};
use super::*;

/// A cache folder of its own, removed afterwards.
struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Nextcloud signed in to the fake server (the password in memory: no keychain in tests).
fn signed_in(server: &Server, name: &str) -> (Nextcloud, Dir) {
    let dir = std::env::temp_dir().join(format!("wordlogic-nextcloud-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let nc = Nextcloud::new(None, Some(dir.clone()), |_, _| {});
    {
        let mut s = lock(&nc.state);
        s.stored.account = Some(Account { server: server.url.clone(), login: USER.into(), user: USER.into() });
        s.password = Some("app-password".into());
    }
    (nc, Dir(dir))
}

fn docx(text: &str) -> Vec<u8> {
    wordcraft_engine::io::save_bytes("x.docx", &Document::from_text(text)).unwrap()
}

fn text_of(bytes: &[u8]) -> String {
    let doc = wordcraft_engine::io::open_bytes("x.docx", bytes).unwrap();
    doc.plain_text(StoryRef::Body)
}

fn body(app: &WordApp) -> String {
    app.session.doc.plain_text(StoryRef::Body)
}

fn app(nc: &Nextcloud) -> WordApp {
    WordApp::new(Session::new(Document::new()), Services { cloud: Some(nc.cloud()), ..Default::default() })
}

/// Runs the UI thread's side (the host, then the File view's answers) until `done`, or fails
/// after a while.
fn until(nc: &mut Nextcloud, app: &mut WordApp, ctx: &egui::Context, what: &str, mut done: impl FnMut(&mut WordApp) -> bool) {
    let start = Instant::now();
    loop {
        nc.poll(app, ctx);
        wordcraft_ui_egui::cloud_files::poll(app);
        if done(app) {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn files_list_open_and_save_back_with_etags() {
    let server = Server::start();
    server.data().put("Docs/Report.docx", docx("Hello"));
    server.data().put("Docs/Budget.xlsx", b"not ours".to_vec());
    server.data().put("Docs/Old/Plan.odt", b"odt".to_vec());
    let (nc, _dir) = signed_in(&server, "files");
    let listing = run(&nc.state, "files.list", &json!({"path": "Docs"}), None, &|_| {}).unwrap();
    let names: Vec<&str> = listing["items"].as_array().unwrap().iter().filter_map(|i| i["name"].as_str()).collect();
    assert_eq!(names, ["Old", "Report.docx"]);

    let opened = run(&nc.state, "files.open", &json!({"path": "Docs/Report.docx", "openPaths": []}), None, &|_| {}).unwrap();
    let local = opened["localPath"].as_str().unwrap().to_string();
    assert_eq!(text_of(&std::fs::read(&local).unwrap()), "Hello");
    assert_eq!((nc.cloud().location_of(&local)).as_deref(), Some("Docs/Report.docx"));
    assert_eq!(opened["shared"], true, "a .docx joins its co-editing session");

    // Saved here: sent over the version it came from.
    std::fs::write(&local, docx("Hello again")).unwrap();
    let remote = lock(&nc.state).stored.files.get(&local).cloned().unwrap();
    send_file(&nc.state, &local, &remote);
    assert_eq!(text_of(&server.data().files["Docs/Report.docx"].bytes), "Hello again");
    let etag = lock(&nc.state).stored.files[&local].etag.clone();
    assert_eq!(etag, Some(format!("\"{}\"", server.data().files["Docs/Report.docx"].etag)), "the new version is remembered");

    // Someone else saved it meanwhile: a conflict, not an overwrite.
    server.data().put("Docs/Report.docx", docx("Their edit"));
    std::fs::write(&local, docx("My edit")).unwrap();
    let remote = lock(&nc.state).stored.files.get(&local).cloned().unwrap();
    send_file(&nc.state, &local, &remote);
    assert_eq!(text_of(&server.data().files["Docs/Report.docx"].bytes), "Their edit");
    assert_eq!(lock(&nc.state).conflicts, [(local.clone(), "Report.docx".to_string())]);
    // Keep mine.
    resolve(&nc.state, &local, "mine").unwrap();
    assert_eq!(text_of(&server.data().files["Docs/Report.docx"].bytes), "My edit");
    assert!(lock(&nc.state).conflicted.is_empty());
}

#[test]
fn save_as_asks_before_replacing_and_names_a_document() {
    let server = Server::start();
    server.data().put("Docs/Report.docx", docx("Hello"));
    let (nc, _dir) = signed_in(&server, "saveas");
    let there = save_as_target(&nc.state, "Docs", "Report", false).unwrap();
    assert_eq!(there, json!({"exists": true, "name": "Report.docx"}));
    let new = save_as_target(&nc.state, "Docs", "Minutes.rtf", false).unwrap();
    assert_eq!(new["name"], "Minutes.rtf", "a format that keeps the document stays");
    let notes = save_as_target(&nc.state, "Docs", "Notes.txt", false).unwrap();
    assert_eq!(notes["name"], "Notes.txt.docx", "Save As writes a whole document");
    assert!(save_as_target(&nc.state, "../x", "Report", false).is_err());
    assert!(save_as_target(&nc.state, "Docs", "a/b", false).is_err());
}

#[test]
fn a_document_from_nextcloud_without_the_app_opens_and_saves_back() {
    let server = Server::start();
    server.data().put("Docs/Notes.docx", docx("First line"));
    // No WordLogic app on this server: joining fails, and the document opens unshared.
    server.data().no_app = true;
    let (mut nc, _dir) = signed_in(&server, "unshared");
    let ctx = egui::Context::default();
    let mut app = app(&nc);
    wordcraft_ui_egui::cloud_files::open_remote(&mut app, "Docs/Gone.docx");
    until(&mut nc, &mut app, &ctx, "an error for a missing file", |app| app.cloud_files.status.as_ref().is_some_and(|s| s.1));
    wordcraft_ui_egui::cloud_files::open_remote(&mut app, "Docs/Notes.docx");
    until(&mut nc, &mut app, &ctx, "the document to open", |app| body(app) == "First line");
    assert!(!app.session.collab_active());
    assert_eq!(app.cloud_location().as_deref(), Some("Docs/Notes.docx"));
    assert!(app.autosaves(), "on for a document in Nextcloud");
    app.execute("caret.docEnd", json!({})).unwrap();
    app.execute("text.insert", json!({"text": " and more"})).unwrap();
    nc.poll(&mut app, &ctx);
    // AutoSave's save (or Save): the file goes to Nextcloud, over the version it came from.
    app.execute("file.save", json!({})).unwrap();
    until(&mut nc, &mut app, &ctx, "the save in Nextcloud", |_| text_of(&server.data().files["Docs/Notes.docx"].bytes) == "First line and more");
    // Someone else changes it; the next save here is a conflict for the user to settle.
    server.data().put("Docs/Notes.docx", docx("Theirs"));
    app.execute("text.insert", json!({"text": "!"})).unwrap();
    nc.poll(&mut app, &ctx);
    app.execute("file.save", json!({})).unwrap();
    until(&mut nc, &mut app, &ctx, "the conflict", |app| !app.cloud_files.conflicts.is_empty());
    assert_eq!(text_of(&server.data().files["Docs/Notes.docx"].bytes), "Theirs", "not overwritten");
}

/// The browser's WordLogic: an engine session sharing the same document through the log.
struct Browser {
    s: Session,
}

impl Browser {
    /// Sends its edit to the server's log and takes the server's answer back.
    fn push(&mut self, server: &Server, path: &str) {
        let out = self.s.collab_outgoing().unwrap();
        let op = serde_json::to_string(&out.op).unwrap();
        let seq = {
            let mut d = server.data();
            let session = d.session_of(path).unwrap();
            let seq = session.log.last().map_or(session.checkpoint_seq, |e| e.0) + 1;
            session.log.push((seq, out.client, out.client_seq, out.base, op));
            seq
        };
        self.s.collab_receive(Incoming { seq, client: out.client, client_seq: out.client_seq, base: out.base, op: out.op }).unwrap();
    }

    /// Takes the log entries it hasn't had.
    fn catch_up(&mut self, server: &Server, path: &str) {
        let entries = server.data().session_of(path).unwrap().log.clone();
        for (seq, client, client_seq, base, op) in entries {
            if seq > self.s.collab_seq().unwrap() {
                let op = serde_json::from_str(&op).unwrap();
                self.s.collab_receive(Incoming { seq, client, client_seq, base, op }).unwrap();
            }
        }
    }

    fn text(&self) -> String {
        self.s.doc.plain_text(StoryRef::Body)
    }
}

#[test]
fn co_editing_with_the_browser_from_a_state_snapshot() {
    let path = "Shared/Plan.docx";
    let server = Server::start();
    // The browser opened the file and started the session; it typed, and saved a checkpoint with
    // the engine's snapshot.
    let base = Document::from_text("Plan");
    let etag0 = server.data().put(path, wordcraft_engine::io::save_bytes("x.docx", &base).unwrap());
    let file_id = server.data().files[path].id;
    server.data().sessions.insert(
        file_id,
        super::fake_server::Session {
            file_id,
            checkpoint_etag: etag0,
            next_client: 1,
            members: vec![(1, "Bo Lee".into(), "{}".into(), true, true)],
            ..Default::default()
        },
    );
    let mut browser = Browser { s: Session::new(base) };
    browser.s.collab_start(1, 0).unwrap();
    browser.s.run("caret.docEnd", &json!({})).unwrap();
    browser.s.run("text.insert", &json!({"text": " for the year"})).unwrap();
    browser.push(&server, path);
    let (seq, state) = browser.s.collab_snapshot().unwrap().unwrap();
    // The file as the checkpoint wrote it. Its text isn't the shared document's, so the test
    // tells the snapshot from the file.
    let etag = server.data().put(path, docx("What the file says"));
    {
        let mut d = server.data();
        let session = d.session_of(path).unwrap();
        session.checkpoint_seq = seq;
        session.checkpoint_etag = etag;
        session.snapshot = Some((seq, state));
    }
    // More typing after the checkpoint.
    browser.s.run("text.insert", &json!({"text": "!"})).unwrap();
    browser.push(&server, path);

    // The desktop opens it from Nextcloud and joins.
    let (mut nc, _dir) = signed_in(&server, "coedit");
    let ctx = egui::Context::default();
    let mut app = app(&nc);
    wordcraft_ui_egui::cloud_files::open_remote(&mut app, path);
    until(&mut nc, &mut app, &ctx, "the shared document", |app| app.session.collab_active());
    assert_eq!(body(&app), browser.text(), "the snapshot, then the log");
    assert_eq!(body(&app), "Plan for the year!");

    // The desktop types; the browser gets it.
    app.execute("caret.docEnd", json!({})).unwrap();
    app.execute("text.insert", json!({"text": " Signed off."})).unwrap();
    let desktop = app.session.collab_client().unwrap();
    until(&mut nc, &mut app, &ctx, "the desktop's edit in the log", |_| server.data().session_of(path).unwrap().log.iter().any(|e| e.1 == desktop));
    browser.catch_up(&server, path);
    assert_eq!(browser.text(), "Plan for the year! Signed off.");

    // The browser types; the desktop gets it, and its caret stays where it was.
    browser.s.run("caret.docStart", &json!({})).unwrap();
    browser.s.run("text.insert", &json!({"text": "2027 "})).unwrap();
    browser.push(&server, path);
    until(&mut nc, &mut app, &ctx, "the browser's edit on the desktop", |app| body(app) == "2027 Plan for the year! Signed off.");
    app.execute("text.insert", json!({"text": " Thanks"})).unwrap();
    assert_eq!(body(&app), "2027 Plan for the year! Signed off. Thanks");

    // The desktop's caret reaches the others.
    until(&mut nc, &mut app, &ctx, "the desktop's position", |_| {
        server.data().session_of(path).unwrap().members.iter().any(|m| m.0 == desktop && m.2.contains("anchor"))
    });

    // The browser's caret reaches the desktop: drawn on the page in the page's colour for it,
    // with its name.
    let at = browser.s.collab_presence().to_string();
    server.data().session_of(path).unwrap().members.iter_mut().filter(|m| m.0 == 1).for_each(|m| m.2 = at.clone());
    until(&mut nc, &mut app, &ctx, "the browser's caret on the desktop", |app| !app.presence.is_empty());
    assert_eq!(app.presence[0].name, "Bo Lee");
    assert_eq!(app.presence[0].color, wordcraft_ui_egui::presence::color_for(1));
    assert_eq!(app.session.collab_others()[0].focus, browser.s.sel.focus);

    // Save writes a checkpoint: the file, and the snapshot of the shared document beside it.
    until(&mut nc, &mut app, &ctx, "the last edit confirmed", |app| !app.session.collab_pending());
    let typed = app.session.collab_seq().unwrap();
    app.execute("file.save", json!({})).unwrap();
    until(&mut nc, &mut app, &ctx, "the checkpoint", |_| !server.data().session_of(path).unwrap().checkpoints.is_empty());
    let (by, at, _, state) = server.data().session_of(path).unwrap().checkpoints[0].clone();
    assert_eq!(by, desktop);
    assert_eq!(text_of(&server.data().files[path].bytes), "2027 Plan for the year! Signed off. Thanks");
    let snapshot = wordcraft_engine::collab::from_snapshot(&state).unwrap();
    assert_eq!(snapshot.plain_text(StoryRef::Body), "2027 Plan for the year! Signed off. Thanks");
    // (Saving stamps the document's properties, itself a shared edit after the text.)
    assert!(at >= typed, "the checkpoint has everything typed ({at} < {typed})");
    // The copy here follows the session's saves: opening it again later fetches Nextcloud's.
    let local = app.session.path.as_ref().unwrap().to_string_lossy().to_string();
    let state = nc.state.clone();
    until(&mut nc, &mut app, &ctx, "the copy's record to follow the checkpoint", |_| {
        let saved = format!("\"{}\"", server.data().files[path].etag);
        lock(&state).stored.files.get(&local).is_some_and(|r| r.etag.as_deref() == Some(saved.as_str()))
    });

    // Quitting leaves the session.
    nc.shutdown(&mut app);
    assert!(!app.session.collab_active());
    let start = Instant::now();
    while server.data().session_of(path).unwrap().members.iter().any(|m| m.0 == desktop && m.4) {
        assert!(start.elapsed() < Duration::from_secs(5), "left the session");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(server.data().without_ocs_header.is_empty(), "every app call passes Nextcloud's CSRF check");
}

#[test]
fn joining_from_the_file_when_no_snapshot_is_kept() {
    let path = "Plain.docx";
    let server = Server::start();
    server.data().put(path, docx("Just the file"));
    let (mut nc, _dir) = signed_in(&server, "nosnapshot");
    let ctx = egui::Context::default();
    let mut app = app(&nc);
    wordcraft_ui_egui::cloud_files::open_remote(&mut app, path);
    until(&mut nc, &mut app, &ctx, "the shared document", |app| app.session.collab_active());
    assert_eq!(body(&app), "Just the file");
    assert!(server.data().requests.iter().all(|(_, p)| !p.contains("collab/state")), "nothing to fetch");
    nc.shutdown(&mut app);
}

#[test]
fn the_shared_state_is_dropped_when_another_document_opens() {
    let path = "Shared.docx";
    let server = Server::start();
    server.data().put(path, docx("Shared"));
    let (mut nc, _dir) = signed_in(&server, "replaced");
    let ctx = egui::Context::default();
    let mut app = app(&nc);
    wordcraft_ui_egui::cloud_files::open_remote(&mut app, path);
    until(&mut nc, &mut app, &ctx, "the shared document", |app| app.session.collab_active());
    let client = app.session.collab_client().unwrap();
    app.execute("file.new", json!({})).unwrap();
    until(&mut nc, &mut app, &ctx, "sharing to end", |_| nc_is_unshared(&server, path, client));
    assert!(!app.session.collab_active());
}

/// Whether `client` left the session for `path`.
fn nc_is_unshared(server: &Server, path: &str, client: u64) -> bool {
    server.data().session_of(path).is_some_and(|s| s.members.iter().any(|m| m.0 == client && !m.4))
}

#[test]
fn a_lost_session_is_rejoined_from_the_file() {
    let path = "Rejoin.docx";
    let server = Server::start();
    server.data().put(path, docx("Before"));
    let (mut nc, _dir) = signed_in(&server, "rejoin");
    let ctx = egui::Context::default();
    let mut app = app(&nc);
    wordcraft_ui_egui::cloud_files::open_remote(&mut app, path);
    until(&mut nc, &mut app, &ctx, "the shared document", |app| app.session.collab_active());
    // The server forgets the session (cleaned up, or restarted): this copy can't follow it.
    server.data().sessions.clear();
    server.data().put(path, docx("Changed meanwhile"));
    until(&mut nc, &mut app, &ctx, "a new session, from the file as it is now", |app| {
        app.session.collab_active() && body(app) == "Changed meanwhile"
    });
    assert_eq!(server.data().sessions.len(), 1, "joined again");
    nc.shutdown(&mut app);
}

#[test]
fn a_session_on_a_newer_wordlogic_opens_unshared_and_says_why() {
    let path = "Newer.docx";
    let server = Server::start();
    server.data().put(path, docx("Edited elsewhere"));
    let file_id = server.data().files[path].id;
    let etag = server.data().files[path].etag.clone();
    // Someone joined with a WordLogic whose edits this one would misread.
    server.data().sessions.insert(
        file_id,
        super::fake_server::Session { file_id, checkpoint_etag: etag, next_client: 1, build: (PROTOCOL + 1).to_string(), ..Default::default() },
    );
    let (mut nc, _dir) = signed_in(&server, "newer");
    let ctx = egui::Context::default();
    let mut app = app(&nc);
    wordcraft_ui_egui::cloud_files::open_remote(&mut app, path);
    until(&mut nc, &mut app, &ctx, "the document, unshared", |app| body(app) == "Edited elsewhere" && app.cloud_files.notice.is_some());
    assert!(!app.session.collab_active());
    let (title, text) = app.cloud_files.notice.clone().unwrap();
    assert_eq!((title.as_str(), text.as_str()), ("Opened without editing together", coedit::OUTDATED));
    let mut data = server.data();
    let s = data.session_of(path).unwrap();
    assert_eq!(s.builds.iter().map(|b| b.1.as_str()).collect::<Vec<_>>(), [PROTOCOL.to_string()], "joined saying what its ops mean");
    assert!(s.members.iter().all(|m| !m.4), "it left the session it couldn't follow");
}

#[test]
fn someone_joining_with_a_newer_wordlogic_ends_sharing_without_rejoining() {
    let path = "Outdated.docx";
    let server = Server::start();
    server.data().put(path, docx("Shared"));
    let (mut nc, _dir) = signed_in(&server, "outdated");
    let ctx = egui::Context::default();
    let mut app = app(&nc);
    wordcraft_ui_egui::cloud_files::open_remote(&mut app, path);
    until(&mut nc, &mut app, &ctx, "the shared document", |app| app.session.collab_active());
    // A newer WordLogic joins: the server answers this copy's next call `outdated`.
    server.data().session_of(path).unwrap().build = (PROTOCOL + 1).to_string();
    until(&mut nc, &mut app, &ctx, "sharing to end", |app| !app.session.collab_active() && app.cloud_files.notice.is_some());
    let (title, text) = app.cloud_files.notice.clone().unwrap();
    assert_eq!((title.as_str(), text.as_str()), ("Co-editing stopped", coedit::OUTDATED_NOW));
    assert_eq!(body(&app), "Shared", "the document stays open");
    std::thread::sleep(Duration::from_millis(300));
    nc.poll(&mut app, &ctx);
    let joins = server.data().requests.iter().filter(|(_, p)| p.contains("collab/join")).count();
    assert_eq!(joins, 1, "rejoining couldn't help: it doesn't");
    nc.shutdown(&mut app);
}

#[test]
fn a_desktop_link_opens_the_document_once_signed_in_to_that_server() {
    let server = Server::start();
    server.data().put("Team/Minutes.docx", docx("Minutes"));
    let (mut nc, _dir) = signed_in(&server, "link");
    let ctx = egui::Context::default();
    let mut app = app(&nc);
    let link = format!("wordlogic://open?server={}&path={}", encode(&server.url), encode("/Team/Minutes.docx"));
    assert!(nc.open_link(&link));
    until(&mut nc, &mut app, &ctx, "the linked document", |app| app.session.collab_active());
    assert_eq!(body(&app), "Minutes");
    assert_eq!(app.cloud_location().as_deref(), Some("Team/Minutes.docx"));
    nc.shutdown(&mut app);

    // Another server: File › Account asks to sign in there first, with its address filled in.
    let mut app = self::app(&nc);
    assert!(nc.open_link("wordlogic://open?server=https%3A%2F%2Fother.example&path=Doc.docx"));
    nc.poll(&mut app, &ctx);
    assert!(app.ui.backstage && app.ui.backstage_page == "account");
    assert_eq!(app.cloud_files.server, "other.example");
    assert!(server.data().requests.iter().all(|(_, p)| !p.contains("Doc.docx")), "nothing asked of the wrong server");
}

#[test]
fn chat_opens_the_documents_talk_conversation_once_it_is_shared() {
    let server = Server::start();
    server.data().put("Docs/Report.docx", docx("Hello"));
    let (mut nc, _dir) = signed_in(&server, "talk");
    let mut app = app(&nc);
    let ctx = egui::Context::default();
    let doc = || Doc {
        title: "Report".into(),
        path: None,
        id: 1,
        remote: Some(Remote { path: "Docs/Report.docx".into(), etag: None, synced: None }),
        encode: Box::new(|_| Err("not needed".into())),
    };

    // No Talk on the server: no chat.
    nc.poll(&mut app, &ctx);
    let start = Instant::now();
    while lock(&nc.state).talk.is_none() {
        assert!(start.elapsed() < Duration::from_secs(20), "timed out asking about Talk");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!nc.cloud().has_talk());

    // With Talk (asked again for another sign-in): chat is offered.
    server.data().talk = true;
    lock(&nc.state).talk = None;
    nc.talk_asked_at = f64::NEG_INFINITY;
    nc.poll(&mut app, &ctx);
    let start = Instant::now();
    while !nc.cloud().has_talk() {
        assert!(start.elapsed() < Duration::from_secs(20), "timed out asking about Talk");
        std::thread::sleep(Duration::from_millis(20));
    }

    // Not shared with anyone: Talk has no conversation for it, and the user is told to share it.
    let opened = Mutex::new(Vec::<String>::new());
    let open = |url: &str| opened.lock().unwrap().push(url.to_string());
    let e = run(&nc.state, "talk.open", &json!({}), Some(doc()), &open).unwrap_err();
    assert!(e.contains("Share it with someone"), "{e}");
    assert!(opened.lock().unwrap().is_empty());

    // Shared: its conversation opens in the browser.
    let id = server.data().files["Docs/Report.docx"].id;
    server.data().talk_rooms.push(id);
    let answer = run(&nc.state, "talk.open", &json!({}), Some(doc()), &open).unwrap();
    let url = format!("{}/index.php/call/room{id}", server.url);
    assert_eq!(answer, json!({"url": url}));
    assert_eq!(*opened.lock().unwrap(), [url]);
    assert!(server.data().without_ocs_header.is_empty(), "OCS calls carry OCS-APIRequest");

    // A document that isn't in Nextcloud has no chat.
    let local = Doc { remote: None, ..doc() };
    assert!(run(&nc.state, "talk.open", &json!({}), Some(local), &open).is_err());
}
