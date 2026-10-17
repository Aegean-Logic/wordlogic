//! The desktop's Nextcloud end to end, against a fake Nextcloud on a local port
//! ([`super::fake_server`]): listing and opening files, sending saves back with ETags, conflicts,
//! and Save As.

use std::time::{Duration, Instant};

use serde_json::json;
use wordcraft_engine::Session;
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
fn a_document_from_nextcloud_opens_and_saves_back() {
    let server = Server::start();
    server.data().put("Docs/Notes.docx", docx("First line"));
    let (mut nc, _dir) = signed_in(&server, "unshared");
    let ctx = egui::Context::default();
    let mut app = app(&nc);
    wordcraft_ui_egui::cloud_files::open_remote(&mut app, "Docs/Gone.docx");
    until(&mut nc, &mut app, &ctx, "an error for a missing file", |app| app.cloud_files.status.as_ref().is_some_and(|s| s.1));
    wordcraft_ui_egui::cloud_files::open_remote(&mut app, "Docs/Notes.docx");
    until(&mut nc, &mut app, &ctx, "the document to open", |app| body(app) == "First line");
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
