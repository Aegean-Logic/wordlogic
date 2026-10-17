//! Nextcloud in the File view, against a fake cloud that answers at once: signing in from the
//! Account page, opening a document from a Nextcloud folder, Save As into one (with Replace),
//! Recent reopening from Nextcloud, AutoSave being on for documents in Nextcloud, and sync
//! conflicts.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use egui_kittest::kittest::Queryable;
use serde_json::{Value as Json, json};
use wordcraft_engine::Session;
use wordcraft_ui_egui::cloud::{Cloud, CloudInbox};
use wordcraft_ui_egui::{Services, WordApp};

/// A fake cloud: a scripted account and folder tree, copies in a temporary folder.
#[derive(Clone)]
struct Fake {
    requests: Arc<Mutex<Vec<(String, Json)>>>,
    signed_in: Arc<Mutex<bool>>,
    /// Local copies tied to cloud files: (local path, cloud path).
    tied: Arc<Mutex<Vec<(String, String)>>>,
    dir: PathBuf,
}

/// Writes a .docx holding `text` at `path`.
fn write_docx(path: &str, text: &str) {
    let mut s = Session::new(wordcraft_doc::Document::new());
    s.run("text.insert", &json!({"text": text})).unwrap();
    s.run("file.save", &json!({"path": path})).unwrap();
}

impl Fake {
    fn new(name: &str, signed_in: bool) -> Fake {
        let dir = std::env::temp_dir().join(format!("wordlogic-cloud-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Fake { requests: Arc::default(), signed_in: Arc::new(Mutex::new(signed_in)), tied: Arc::default(), dir }
    }

    fn cloud(&self) -> Cloud {
        let inbox: CloudInbox = Arc::default();
        let answers = inbox.clone();
        let me = self.clone();
        let tied = self.tied.clone();
        Cloud {
            request: Box::new(move |id, op, params| {
                me.requests.lock().unwrap().push((op.to_string(), params.clone()));
                let account = |on: bool| json!({"signedIn": on, "account": on.then_some("ana on cloud.example"), "server": on.then_some("https://cloud.example")});
                let answer = match op {
                    "account.info" => Ok(account(*me.signed_in.lock().unwrap())),
                    "account.signIn" => {
                        *me.signed_in.lock().unwrap() = true;
                        Ok(account(true))
                    }
                    "files.list" => {
                        let path = params["path"].as_str().unwrap_or("").to_string();
                        Ok(if path.is_empty() {
                            json!({"path": "", "items": [{"name": "Finance", "path": "Finance", "folder": true}]})
                        } else {
                            json!({"path": path, "items": [{"name": "Report.docx", "path": "Finance/Report.docx", "folder": false}]})
                        })
                    }
                    "files.open" => {
                        let local = me.dir.join("Report.docx").to_string_lossy().to_string();
                        if params["openPaths"].as_array().unwrap().iter().any(|p| p == &json!(local)) {
                            Ok(json!({"localPath": local, "alreadyOpen": true}))
                        } else {
                            write_docx(&local, "From the cloud");
                            let mut tied = me.tied.lock().unwrap();
                            tied.retain(|(l, _)| *l != local);
                            tied.push((local.clone(), params["path"].as_str().unwrap().to_string()));
                            Ok(json!({"localPath": local}))
                        }
                    }
                    "files.saveAs" => {
                        let name = params["name"].as_str().unwrap().to_string();
                        if params["replace"] != json!(true) {
                            Ok(json!({"exists": true, "name": format!("{name}.docx")}))
                        } else {
                            let local = me.dir.join(format!("{name}.docx"));
                            me.tied.lock().unwrap().push((local.to_string_lossy().to_string(), format!("Finance/{name}.docx")));
                            Ok(json!({"localPath": local.to_string_lossy(), "name": format!("{name}.docx")}))
                        }
                    }
                    "files.resolve" => {
                        let local = params["localPath"].as_str().unwrap().to_string();
                        match params["choice"].as_str() {
                            Some("theirs") => {
                                // Their version replaces the copy here.
                                write_docx(&local, "Their version");
                                Ok(json!({"localPath": local, "reload": true}))
                            }
                            Some("keepBoth") => {
                                let copy = me.dir.join("Report (copy).docx").to_string_lossy().to_string();
                                me.tied.lock().unwrap().retain(|(l, _)| *l != local);
                                me.tied.lock().unwrap().push((copy.clone(), "Finance/Report (copy).docx".into()));
                                Ok(json!({"localPath": copy, "saveAs": true, "name": "Report (copy).docx"}))
                            }
                            _ => Ok(json!({"name": "Report.docx"})),
                        }
                    }
                    other => Err(format!("unexpected {other}")),
                };
                answers.lock().unwrap().push((id, answer));
            }),
            inbox,
            files: true,
            location: Some(Box::new(move |local: &str| tied.lock().unwrap().iter().find(|(l, _)| l == local).map(|(_, r)| r.clone()))),
        }
    }

    fn ops(&self) -> Vec<(String, Json)> {
        self.requests.lock().unwrap().clone()
    }

    fn asked(&self, op: &str) -> Vec<Json> {
        self.ops().into_iter().filter(|(o, _)| o == op).map(|(_, p)| p).collect()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        if Arc::strong_count(&self.requests) == 1 {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn app_with(cloud: Option<Cloud>) -> WordApp {
    WordApp::new(Session::new(wordcraft_doc::Document::new()), Services { cloud, ..Default::default() })
}

fn harness(fake: &Fake) -> egui_kittest::Harness<'static, WordApp> {
    let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1440.0, 1000.0)).build_ui_state(
        |ui, app: &mut WordApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        app_with(Some(fake.cloud())),
    );
    h.run_steps(4);
    h
}

fn page(h: &mut egui_kittest::Harness<'static, WordApp>, page: &str) {
    let r = h.state_mut().run("ui.backstage", json!({"value": true, "page": page}));
    assert!(r.is_ok(), "{r:?}");
    h.run_steps(6);
}

fn body(app: &WordApp) -> String {
    app.session.doc.plain_text(wordcraft_doc::StoryRef::Body)
}

/// Opens Finance/Report.docx from the fake Nextcloud through File › Open.
fn open_report(h: &mut egui_kittest::Harness<'static, WordApp>) {
    page(h, "open");
    h.get_by_label("Finance").click();
    h.run_steps(6);
    h.get_by_label("Report.docx").click();
    h.run_steps(8);
}

#[test]
fn sign_in_from_the_account_page() {
    let fake = Fake::new("signin", false);
    let mut h = harness(&fake);
    page(&mut h, "account");
    h.get_by_label("Nextcloud server address").click();
    h.get_by_label("Nextcloud server address").type_text("cloud.example");
    h.run_steps(2);
    h.get_by_label("Sign in to Nextcloud…").click();
    h.run_steps(6);
    assert_eq!(fake.asked("account.signIn"), [json!({"server": "cloud.example"})]);
    assert!(h.query_by_label_contains("Signed in as ana on cloud.example").is_some());
    assert!(h.query_by_label("Sign out").is_some());
}

#[test]
fn open_a_document_from_a_nextcloud_folder() {
    let fake = Fake::new("open", true);
    let mut h = harness(&fake);
    open_report(&mut h);
    assert_eq!(fake.asked("files.open")[0]["path"], "Finance/Report.docx");
    let app = h.state();
    assert!(!app.ui.backstage, "the document shows");
    assert!(body(app).contains("From the cloud"));
    assert_eq!(app.cloud_location().as_deref(), Some("Finance/Report.docx"));
    assert!(app.autosaves(), "documents in Nextcloud AutoSave, though only opened");
    // Opening it again finds it open: nothing is fetched over it.
    open_report(&mut h);
    assert_eq!(fake.asked("files.open").len(), 2);
    assert!(!h.state().ui.backstage);
}

#[test]
fn a_local_document_isnt_autosaved_until_saved_here() {
    let fake = Fake::new("local", true);
    let mut h = harness(&fake);
    let local = fake.dir.join("Local.docx").to_string_lossy().to_string();
    write_docx(&local, "Here");
    h.state_mut().run("file.open", json!({"path": local})).unwrap();
    h.run_steps(2);
    assert_eq!(h.state().cloud_location(), None);
    assert!(!h.state().autosaves(), "a file only opened isn't rewritten (#196)");
}

#[test]
fn save_as_into_nextcloud_asks_before_replacing() {
    let fake = Fake::new("saveas", true);
    let mut h = harness(&fake);
    h.state_mut().run("text.insert", json!({"text": "Forecast figures"})).unwrap();
    page(&mut h, "saveAs");
    h.get_by_label("Finance").click();
    h.run_steps(6);
    // The name starts as the document's own; type another.
    assert_eq!(h.state().cloud_files.name, "Document1");
    h.state_mut().cloud_files.name.clear();
    h.run_steps(2);
    h.get_by_label("Nextcloud file name").click();
    h.get_by_label("Nextcloud file name").type_text("Forecast");
    h.run_steps(2);
    h.get_by_label("Save to Nextcloud").click();
    h.run_steps(6);
    assert!(h.query_by_label_contains("Forecast.docx is already there").is_some());
    h.get_by_label("Replace").click();
    h.run_steps(8);
    assert_eq!(
        fake.asked("files.saveAs"),
        [json!({"folder": "Finance", "name": "Forecast", "replace": false}), json!({"folder": "Finance", "name": "Forecast", "replace": true})]
    );
    let app = h.state();
    assert!(app.session.path.as_deref().is_some_and(|p| p.ends_with("Forecast.docx")) && !app.session.dirty, "saved to its copy");
    assert_eq!(app.cloud_location().as_deref(), Some("Finance/Forecast.docx"));
    assert!(!app.ui.backstage);
}

#[test]
fn autosave_can_be_switched_off_for_one_nextcloud_document() {
    let fake = Fake::new("autosave", true);
    let mut h = harness(&fake);
    open_report(&mut h);
    assert!(h.state().autosaves());
    h.state_mut().run("file.autosave", json!({})).unwrap();
    assert!(!h.state().autosaves(), "off for this document");
    assert!(h.state().autosave, "the AutoSave setting for other documents is unchanged");
    h.state_mut().run("file.autosave", json!({"value": true})).unwrap();
    assert!(h.state().autosaves());
}

#[test]
fn recent_reopens_a_nextcloud_document_from_nextcloud() {
    let fake = Fake::new("recent", true);
    let mut h = harness(&fake);
    open_report(&mut h);
    h.state_mut().run("file.new", json!({})).unwrap();
    h.run_steps(2);
    page(&mut h, "home");
    // Listed as where it is in Nextcloud, not by the copy's path in the cache folder.
    h.get_by_label("Report.docx — Nextcloud › Finance/Report.docx").click();
    h.run_steps(8);
    let opens = fake.asked("files.open");
    assert_eq!(opens.len(), 2, "fetched from Nextcloud again");
    assert_eq!(opens[1]["path"], "Finance/Report.docx");
    assert!(body(h.state()).contains("From the cloud"));
}

/// Opens Finance/Report.docx from the fake Nextcloud, types an edit, saves, and has the host
/// report that someone else changed the file meanwhile.
fn conflicted(name: &str) -> (Fake, egui_kittest::Harness<'static, WordApp>, String) {
    let fake = Fake::new(name, true);
    let mut h = harness(&fake);
    open_report(&mut h);
    h.state_mut().run("caret.docEnd", json!({})).unwrap();
    h.state_mut().run("text.insert", json!({"text": " My change"})).unwrap();
    h.state_mut().run("file.save", json!({})).unwrap();
    let local = h.state().session.path.as_ref().unwrap().to_string_lossy().to_string();
    h.state_mut().cloud_files.conflicts.push(wordcraft_ui_egui::cloud_files::Conflict { local: local.clone(), name: "Report.docx".into() });
    h.run_steps(4);
    assert!(h.query_by_label_contains("Report.docx changed in Nextcloud").is_some(), "the user is asked");
    (fake, h, local)
}

#[test]
fn a_conflict_can_keep_both_versions() {
    let (fake, mut h, local) = conflicted("keepboth");
    h.get_by_label("Keep both").click();
    h.run_steps(8);
    assert_eq!(fake.asked("files.resolve"), [json!({"localPath": local, "choice": "keepBoth"})]);
    let app = h.state();
    assert!(app.cloud_files.conflicts.is_empty() && h.query_by_label_contains("changed in Nextcloud").is_none(), "asked once");
    assert!(app.session.path.as_deref().is_some_and(|p| p.ends_with("Report (copy).docx")), "the document is now the copy");
    assert!(body(app).contains("My change"), "with the changes made here");
    assert_eq!(app.cloud_location().as_deref(), Some("Finance/Report (copy).docx"));
}

#[test]
fn a_conflict_can_take_nextclouds_version() {
    let (_fake, mut h, local) = conflicted("theirs");
    h.get_by_label("Use Nextcloud's").click();
    h.run_steps(8);
    let app = h.state();
    assert_eq!(app.session.path.as_ref().map(|p| p.to_string_lossy().to_string()), Some(local));
    assert!(body(app).contains("Their version"));
    assert!(!body(app).contains("My change"), "the change made here is gone");
    assert!(app.dialog.is_none(), "nothing asked about the changes dropped");
}

#[test]
fn a_conflict_can_keep_mine() {
    let (fake, mut h, local) = conflicted("mine");
    h.get_by_label("Use mine").click();
    h.run_steps(8);
    assert_eq!(fake.asked("files.resolve"), [json!({"localPath": local, "choice": "mine"})]);
    assert!(body(h.state()).contains("My change"));
}

#[test]
fn problems_are_told_wherever_the_user_is() {
    let fake = Fake::new("notice", true);
    let mut h = harness(&fake);
    h.state_mut().cloud_files.notice = Some(("Couldn't save to Nextcloud".into(), "Nextcloud answered 507.".into()));
    h.run_steps(3);
    assert!(h.query_by_label_contains("Nextcloud answered 507.").is_some());
    h.get_by_label("OK").click();
    h.run_steps(3);
    assert!(h.state().cloud_files.notice.is_none());
}

#[test]
fn the_account_page_is_only_where_files_can_be_opened_from_the_cloud() {
    let fake = Fake::new("nav", true);
    let mut h = harness(&fake);
    page(&mut h, "home");
    assert!(h.query_by_label("Account").is_some(), "File › Account on the desktop");
    // The browser's cloud is for sharing only: the page owns the files.
    let cloud = Cloud::sharing(Box::new(|_, _, _| {}), CloudInbox::default());
    let app = app_with(Some(cloud));
    assert!(!wordcraft_ui_egui::cloud_files::available(&app));
    assert!(!wordcraft_ui_egui::cloud_files::available(&app_with(None)));
}
