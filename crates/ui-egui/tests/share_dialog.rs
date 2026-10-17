//! The Share dialog, driven against a fake cloud that answers at once: who you share with and
//! how, sending, copying the link, managing access, and the desktop's sign-in and upload steps.
//! (Ported from GridLogic's, whose dialog this is.)

use std::sync::{Arc, Mutex};

use egui::vec2;
use egui_kittest::kittest::Queryable;
use serde_json::{Value as Json, json};
use wordcraft_engine::Session;
use wordcraft_ui_egui::cloud::{Cloud, CloudInbox};
use wordcraft_ui_egui::{Services, WordApp};

/// A cloud with a scripted state, recording every request.
#[derive(Clone)]
struct Fake {
    requests: Arc<Mutex<Vec<(String, Json)>>>,
    info: Arc<Mutex<Json>>,
    /// The file's shares, as `share.list` answers.
    shares: Arc<Mutex<Json>>,
}

impl Fake {
    fn new(info: Json) -> Fake {
        let shares = json!([
            {"id": 7, "type": "user", "shareWith": "ben", "label": "Ben", "edit": true, "password": false},
            {"id": 9, "type": "link", "shareWith": "", "label": "", "edit": false, "url": "https://cloud.example/s/abc", "password": false}
        ]);
        Fake { requests: Arc::default(), info: Arc::new(Mutex::new(info)), shares: Arc::new(Mutex::new(shares)) }
    }
    fn without_shares(info: Json) -> Fake {
        let f = Fake::new(info);
        *f.shares.lock().unwrap() = json!([]);
        f
    }
    fn cloud(&self) -> Cloud {
        let inbox: CloudInbox = Arc::default();
        let answers = inbox.clone();
        let me = self.clone();
        Cloud::sharing(
            Box::new(move |id, op, params| {
                me.requests.lock().unwrap().push((op.to_string(), params.clone()));
                let answer = match op {
                    "share.info" => me.info.lock().unwrap().clone(),
                    "account.signIn" => {
                        *me.info.lock().unwrap() = json!({"name": "Report.docx", "signedIn": true, "inCloud": false, "canShare": true});
                        me.info.lock().unwrap().clone()
                    }
                    "share.upload" => {
                        *me.info.lock().unwrap() = json!({"name": "Report.docx", "signedIn": true, "inCloud": true, "canShare": true});
                        me.info.lock().unwrap().clone()
                    }
                    "share.search" => json!([
                        {"label": "Ana Lopez", "detail": "ana@example.com", "type": "user", "shareWith": "ana"},
                        {"label": "Finance", "type": "group", "shareWith": "finance"}
                    ]),
                    "share.list" => me.shares.lock().unwrap().clone(),
                    "share.link" => json!({"id": 9, "type": "link", "url": "https://cloud.example/s/abc", "edit": params["edit"]}),
                    _ => json!({}),
                };
                answers.lock().unwrap().push((id, Ok(answer)));
            }),
            inbox,
        )
    }
    fn ops(&self) -> Vec<(String, Json)> {
        self.requests.lock().unwrap().clone()
    }
}

fn harness(fake: &Fake) -> egui_kittest::Harness<'static, WordApp> {
    let session = Session::new(wordcraft_doc::Document::from_text("Quarterly report"));
    let services = Services { cloud: Some(fake.cloud()), ..Default::default() };
    let mut h = egui_kittest::Harness::builder().with_size(vec2(1440.0, 900.0)).build_ui_state(
        |ui, app: &mut WordApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        WordApp::new(session, services),
    );
    h.run_steps(6);
    h
}

fn open(h: &mut egui_kittest::Harness<'static, WordApp>) {
    let r = h.state_mut().run("ui.share", json!({}));
    assert!(r.is_ok(), "{r:?}");
    h.run_steps(6);
}

fn shared_file() -> Json {
    json!({"name": "Report.docx", "signedIn": true, "inCloud": true, "canShare": true})
}

#[test]
fn share_with_people_and_send() {
    let fake = Fake::new(shared_file());
    let mut h = harness(&fake);
    open(&mut h);
    assert!(h.query_by_label_contains("Report.docx").is_some(), "the title names the document");
    assert!(h.query_by_label_contains("Shared with Ben").is_some(), "who has access, from the cloud");

    // Type a name: suggestions come from the cloud's search.
    h.get_by_label("Add a name, group, or email").click();
    h.run_steps(2);
    h.get_by_label("Add a name, group, or email").type_text("an");
    for _ in 0..20 {
        h.run_steps(2);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    assert!(fake.ops().iter().any(|(op, p)| op == "share.search" && p["query"] == "an"), "{:?}", fake.ops());
    h.get_by_label_contains("Ana Lopez").click();
    h.run_steps(4);

    // A message, then Send: one share per person, with the permission and the message.
    h.get_by_label("Add a message").click();
    h.get_by_label("Add a message").type_text("Please check Q3");
    h.run_steps(2);
    h.get_by_label("  Send  ").click();
    h.run_steps(6);
    let adds: Vec<Json> = fake.ops().into_iter().filter(|(op, _)| op == "share.add").map(|(_, p)| p).collect();
    assert_eq!(adds, vec![json!({"type": "user", "shareWith": "ana", "edit": true, "note": "Please check Q3"})]);
    assert!(h.query_by_label_contains("Shared. They'll get a notification.").is_some());
}

#[test]
fn copy_link() {
    // A link that exists already keeps what it allows (here: view).
    let fake = Fake::new(shared_file());
    let mut h = harness(&fake);
    open(&mut h);
    assert!(h.query_by_label_contains("Link permission: Can view").is_some(), "the link's permission is in plain sight");
    h.get_by_label("Copy link").click();
    h.run_steps(6);
    let link = fake.ops().into_iter().find(|(op, _)| op == "share.link").map(|(_, p)| p);
    assert_eq!(link, Some(json!({"edit": false, "expiration": null})), "copying doesn't change an existing view link");
    assert!(h.query_by_label_contains("Link copied. Anyone with it can view.").is_some());
}

#[test]
fn a_new_link_lets_people_edit() {
    let fake = Fake::without_shares(shared_file());
    let mut h = harness(&fake);
    open(&mut h);
    h.get_by_label("Copy link").click();
    h.run_steps(6);
    let link = fake.ops().into_iter().find(|(op, _)| op == "share.link").map(|(_, p)| p);
    assert_eq!(link, Some(json!({"edit": true, "expiration": null})), "as OneDrive's anyone-with-the-link");
    assert!(h.query_by_label_contains("Link copied. Anyone with it can edit.").is_some());
}

#[test]
fn changing_the_link_permission_changes_the_link() {
    let fake = Fake::new(shared_file());
    let mut h = harness(&fake);
    open(&mut h);
    h.get_by_label("Link permission: Can view").click();
    h.run_steps(2);
    // The open list's "Can edit" choice.
    if let Some(choice) = h.get_all_by_label("Can edit").last() {
        choice.click();
    }
    h.run_steps(6);
    let update = fake.ops().into_iter().find(|(op, _)| op == "share.update").map(|(_, p)| p);
    assert_eq!(update, Some(json!({"id": 9, "edit": true})));
    assert!(h.query_by_label_contains("Link changed: anyone with it can edit.").is_some());
}

#[test]
fn manage_access() {
    let fake = Fake::new(shared_file());
    let mut h = harness(&fake);
    open(&mut h);
    h.get_by_label_contains("Shared with Ben").click();
    h.run_steps(4);
    assert!(h.query_by_label_contains("People with access").is_some());
    if let Some(n) = h.get_all_by_label("Remove").next() {
        n.click();
    }
    h.run_steps(4);
    assert!(fake.ops().iter().any(|(op, p)| op == "share.remove" && p["id"] == 7), "{:?}", fake.ops());
}

#[test]
fn desktop_signs_in_then_uploads_before_sharing() {
    let fake = Fake::new(json!({"name": "Report.docx", "signedIn": false, "inCloud": false, "canShare": true}));
    let mut h = harness(&fake);
    open(&mut h);
    h.get_by_label("Nextcloud server address").click();
    h.get_by_label("Nextcloud server address").type_text("cloud.example");
    h.run_steps(2);
    h.get_by_label("Sign in to Nextcloud…").click();
    h.run_steps(6);
    h.get_by_label("Upload to Nextcloud").click();
    h.run_steps(6);
    assert!(h.query_by_label("Add a name, group, or email").is_some(), "uploaded: ready to share");
    let ops: Vec<String> = fake.ops().into_iter().map(|(op, _)| op).collect();
    assert_eq!(&ops[..3], ["share.info", "account.signIn", "share.upload"]);
    assert_eq!(fake.ops()[1].1, json!({"server": "cloud.example"}), "signs in where the user said");
}

#[test]
fn a_link_guest_can_only_pass_the_link_on() {
    let fake = Fake::new(json!({"name": "Report.docx", "signedIn": true, "inCloud": true, "canShare": false, "link": "https://cloud.example/s/abc"}));
    let mut h = harness(&fake);
    open(&mut h);
    assert!(h.query_by_label("Add a name, group, or email").is_none(), "guests don't share");
    h.get_by_label("Copy link").click();
    h.run_steps(4);
    assert!(h.query_by_label_contains("Link copied.").is_some());
    let ops: Vec<String> = fake.ops().into_iter().map(|(op, _)| op).collect();
    assert_eq!(ops, ["share.info"], "nothing asked of the cloud but the document's info");
}

#[test]
fn without_a_cloud_share_is_the_export_menu() {
    let mut app = WordApp::new(Session::new(wordcraft_doc::Document::from_text("x")), Services::default());
    assert!(app.run("ui.share", json!({})).is_err(), "no cloud: no Share dialog");
}
