//! Headless UI screenshots: renders the whole WordCraft window offscreen (wgpu), so it works
//! with a locked screen or a hidden window.
//!
//! `cargo run -p wordcraft-ui-egui --example ui_shot -- script.jsonl`
//!
//! The script is JSON lines: control-channel requests (`{"method": …, "params": …}`, see
//! `docs/control-protocol.md`), `{"shot": "/abs/out.png"}` to save the window as PNG, or
//! `{"steps": n}` to run extra frames, or `{"wait": ms}` to keep running frames for a while (for
//! background work such as a Zotero session). The window is 1440×900 pt at 2× and opens the sample
//! document unless the first line is `{"empty": true}`; `{"empty": true, "startScreen": true}`
//! starts the way the desktop app does with nothing to open (File › Home, #766). `"fakeCloud": true`
//! on the first line gives the window a scripted Nextcloud (signed in as ana, a few folders and
//! documents) to look at the File view's Nextcloud places; `"fakeCloud": "signedOut"` starts signed
//! out.

use wordcraft_engine::Session;
use wordcraft_ui_egui::{ControlRequest, Services, WordApp};

static READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn main() {
    let path = std::env::args().nth(1).expect("usage: ui_shot script.jsonl");
    let script = std::fs::read_to_string(&path).expect("read script");
    let (tx, rx) = std::sync::mpsc::channel::<ControlRequest>();
    let lines: Vec<serde_json::Value> =
        script.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).expect("json line")).collect();
    let doc = if lines.first().is_some_and(|l| l.get("empty").is_some()) {
        wordcraft_doc::Document::new()
    } else {
        wordcraft_engine::sample::sample_document()
    };
    let cloud = lines.first().and_then(|l| l.get("fakeCloud")).map(|v| fake_cloud(v.as_str() != Some("signedOut")));
    let mut app = WordApp::new(Session::new(doc), Services { cloud, ..Services::default() }).with_control(rx);
    app.integrated_titlebar = true;
    if lines.first().is_some_and(|l| l.get("startScreen").is_some()) {
        app.start_screen();
    }
    let w = lines.iter().find_map(|l| l.get("width").and_then(|v| v.as_f64())).unwrap_or(1440.0) as f32;
    let h = lines.iter().find_map(|l| l.get("height").and_then(|v| v.as_f64())).unwrap_or(900.0) as f32;
    let mut harness =
        egui_kittest::Harness::builder().with_size(egui::vec2(w, h)).with_pixels_per_point(2.0).with_max_steps(1_000_000).wgpu().build_ui_state(
            |ui, app: &mut WordApp| {
                if !READY.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                app.ui(ui);
            },
            app,
        );
    harness.input_mut().max_texture_side = Some(8192);
    READY.store(true, std::sync::atomic::Ordering::Relaxed);
    step_n(&mut harness, 6);
    for l in lines {
        if let Some(p) = l.get("shot").and_then(|v| v.as_str()) {
            step_n(&mut harness, 4);
            match harness.render() {
                Ok(img) => {
                    img.save(p).expect("save png");
                    println!("{p}");
                }
                Err(e) => eprintln!("render failed: {e}"),
            }
        } else if let Some(n) = l.get("steps").and_then(|v| v.as_u64()) {
            step_n(&mut harness, n as usize);
        } else if let Some(ms) = l.get("wait").and_then(|v| v.as_u64()) {
            let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            while std::time::Instant::now() < end {
                step(&mut harness);
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
        } else if let Some(m) = l.get("method").and_then(|v| v.as_str()) {
            let (req, reply) = ControlRequest::new(m, l.get("params").cloned().unwrap_or_default());
            tx.send(req).expect("send");
            for _ in 0..60 {
                step(&mut harness);
                if let Ok(r) = reply.try_recv() {
                    let s = r.to_string();
                    // `UI_SHOT_FULL=1` prints whole replies (e.g. `document.inspect` for scripted checks).
                    let cut = if std::env::var_os("UI_SHOT_FULL").is_some() { s.len() } else { s.len().min(300) };
                    println!("{m}: {}", s.get(..cut).unwrap_or(&s));
                    break;
                }
            }
            step_n(&mut harness, 2);
        }
    }
}

fn step(harness: &mut egui_kittest::Harness<'_, WordApp>) {
    let mut raw = std::mem::take(harness.input_mut());
    harness.state_mut().raw_input_hook(&mut raw);
    *harness.input_mut() = raw;
    harness.step();
}

fn step_n(harness: &mut egui_kittest::Harness<'_, WordApp>, n: usize) {
    for _ in 0..n {
        step(harness);
    }
}

/// A scripted Nextcloud for looking at the File view (`"fakeCloud"` on the first line).
fn fake_cloud(signed_in: bool) -> wordcraft_ui_egui::cloud::Cloud {
    use serde_json::json;
    let inbox = wordcraft_ui_egui::cloud::CloudInbox::default();
    let answers = inbox.clone();
    let signed_in = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(signed_in));
    wordcraft_ui_egui::cloud::Cloud {
        request: Box::new(move |id, op, params| {
            let on = signed_in.load(std::sync::atomic::Ordering::Relaxed);
            let account = |on: bool| json!({"signedIn": on, "account": on.then_some("ana on cloud.example.com"), "server": on.then_some("https://cloud.example.com")});
            let answer = match op {
                "account.info" => Ok(account(on)),
                "account.signIn" => {
                    signed_in.store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(account(true))
                }
                "account.signOut" => {
                    signed_in.store(false, std::sync::atomic::Ordering::Relaxed);
                    Ok(account(false))
                }
                "files.list" => {
                    let path = params["path"].as_str().unwrap_or("").to_string();
                    let items = if path.is_empty() {
                        json!([
                            {"name": "Documents", "path": "Documents", "folder": true},
                            {"name": "Team", "path": "Team", "folder": true},
                            {"name": "WordLogic", "path": "WordLogic", "folder": true},
                            {"name": "Reading list.md", "path": "Reading list.md", "folder": false},
                        ])
                    } else {
                        json!([
                            {"name": "Drafts", "path": format!("{path}/Drafts"), "folder": true},
                            {"name": "Annual report.docx", "path": format!("{path}/Annual report.docx"), "folder": false},
                            {"name": "Board minutes.odt", "path": format!("{path}/Board minutes.odt"), "folder": false},
                            {"name": "Letter to members.rtf", "path": format!("{path}/Letter to members.rtf"), "folder": false},
                        ])
                    };
                    Ok(json!({"path": path, "items": items}))
                }
                other => Err(format!("The scripted Nextcloud doesn't do {other}.")),
            };
            answers.lock().unwrap_or_else(|e| e.into_inner()).push((id, answer));
        }),
        inbox,
        files: true,
        location: None,
        talk: None,
    }
}
