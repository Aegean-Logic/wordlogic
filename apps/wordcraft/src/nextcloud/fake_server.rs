//! A fake Nextcloud for tests, on a local port: WebDAV for one user's files (PROPFIND, GET, HEAD,
//! PUT with ETags and `If-Match` / `If-None-Match`) and the WordLogic app's co-editing sequencer
//! (`/index.php/apps/wordcraft/collab/*`, as `wordcraft-nextcloud`'s `CollabService` answers:
//! join pinned to an ETag, state snapshots, push, sync, checkpoint, leave, and the session kept to
//! its newest co-editing protocol). Plain HTTP/1.1 with
//! keep-alive, one thread per connection.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{Value as Json, json};

/// The fake user, and the app password the tests sign in with.
pub const USER: &str = "ana";

#[derive(Clone, Debug)]
pub struct File {
    pub bytes: Vec<u8>,
    pub etag: String,
    pub id: u64,
}

/// A log entry: (seq, client, client_seq, base, op as JSON text).
pub type Entry = (u64, u64, u64, u64, String);

#[derive(Default)]
pub struct Session {
    pub file_id: u64,
    pub log: Vec<Entry>,
    pub checkpoint_seq: u64,
    pub checkpoint_etag: String,
    /// Kept state snapshots by log position (the latest checkpoint's).
    pub snapshot: Option<(u64, String)>,
    pub next_client: u64,
    /// (client, display name, selection, can edit, still in).
    pub members: Vec<(u64, String, String, bool, bool)>,
    /// Checkpoints recorded: (client, seq, etag, state).
    pub checkpoints: Vec<(u64, u64, String, String)>,
    /// What each join said its build was (`build`), by client.
    pub builds: Vec<(u64, String)>,
    /// The newest build that joined (`join` answers it), as the server keeps it: a member on an
    /// older one is answered 409 `{rejoin, outdated}`.
    pub build: String,
}

impl Session {
    /// Whether `client` joined with an older build than the session's newest.
    fn outdated(&self, client: u64) -> bool {
        let mine = self.builds.iter().find(|b| b.0 == client).and_then(|b| b.1.parse::<u32>().ok());
        mine < self.build.parse::<u32>().ok()
    }
}

#[derive(Default)]
pub struct Data {
    /// By path in the user's files.
    pub files: HashMap<String, File>,
    next_etag: u64,
    next_id: u64,
    /// One session per file id.
    pub sessions: HashMap<u64, Session>,
    /// Every request: (method, path with query).
    pub requests: Vec<(String, String)>,
    /// The WordLogic app isn't installed: its routes aren't there.
    pub no_app: bool,
    /// App routes called without `OCS-APIRequest: true` (Nextcloud's CSRF check would refuse
    /// them).
    pub without_ocs_header: Vec<String>,
}

impl Data {
    /// Puts a file there, as another app would (a new ETag).
    pub fn put(&mut self, path: &str, bytes: Vec<u8>) -> String {
        self.next_etag += 1;
        let etag = format!("e{}", self.next_etag);
        let id = match self.files.get(path) {
            Some(f) => f.id,
            None => {
                self.next_id += 1;
                self.next_id
            }
        };
        self.files.insert(path.to_string(), File { bytes, etag: etag.clone(), id });
        etag
    }

    pub fn session_of(&mut self, path: &str) -> Option<&mut Session> {
        let id = self.files.get(path)?.id;
        self.sessions.get_mut(&id)
    }
}

#[derive(Clone)]
pub struct Server {
    pub url: String,
    data: Arc<Mutex<Data>>,
}

impl Server {
    pub fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let data: Arc<Mutex<Data>> = Arc::default();
        let shared = data.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let data = shared.clone();
                std::thread::spawn(move || serve(stream, &data));
            }
        });
        Server { url, data }
    }

    pub fn data(&self) -> MutexGuard<'_, Data> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }
}

struct Request {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_request(r: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut line = String::new();
    if r.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next()?.to_string(), parts.next()?.to_string());
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        r.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut body = Vec::new();
    if headers.get("transfer-encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
        loop {
            let mut size = String::new();
            r.read_line(&mut size).ok()?;
            let n = usize::from_str_radix(size.trim(), 16).ok()?;
            let mut chunk = vec![0; n + 2];
            r.read_exact(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
    } else if let Some(n) = headers.get("content-length").and_then(|n| n.parse::<usize>().ok()) {
        body = vec![0; n];
        r.read_exact(&mut body).ok()?;
    }
    Some(Request { method, target, headers, body })
}

fn serve(stream: TcpStream, data: &Mutex<Data>) {
    let Ok(mut out) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    while let Some(req) = read_request(&mut reader) {
        let (status, headers, body) = {
            let mut d = data.lock().unwrap_or_else(|e| e.into_inner());
            d.requests.push((req.method.clone(), req.target.clone()));
            answer(&mut d, &req)
        };
        let reason = match status {
            200 => "OK",
            201 => "Created",
            204 => "No Content",
            207 => "Multi-Status",
            404 => "Not Found",
            409 => "Conflict",
            412 => "Precondition Failed",
            _ => "Other",
        };
        let mut head = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n", if req.method == "HEAD" { 0 } else { body.len() });
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let sent = out.write_all(head.as_bytes()).and_then(|_| if req.method == "HEAD" { Ok(()) } else { out.write_all(&body) });
        if sent.is_err() {
            return;
        }
    }
}

type Answer = (u16, Vec<(String, String)>, Vec<u8>);

fn json_answer(status: u16, v: Json) -> Answer {
    (status, vec![("Content-Type".into(), "application/json".into())], v.to_string().into_bytes())
}

/// `a=1&b=x` → map (values percent-decoded).
fn query(q: &str) -> HashMap<String, String> {
    q.split('&').filter_map(|p| p.split_once('=')).map(|(k, v)| (k.to_string(), super::decode(&v.replace('+', " ")))).collect()
}

fn answer(d: &mut Data, req: &Request) -> Answer {
    let (path, q) = req.target.split_once('?').unwrap_or((&req.target, ""));
    let dav = format!("/remote.php/dav/files/{USER}/");
    if let Some(rest) = path.strip_prefix(&dav).or_else(|| (path == dav.trim_end_matches('/')).then_some("")) {
        return webdav(d, req, super::decode(rest).trim_matches('/'));
    }
    if let Some(route) = path.strip_prefix("/index.php/apps/wordcraft/collab/").filter(|_| !d.no_app) {
        if req.headers.get("ocs-apirequest").map(String::as_str) != Some("true") {
            d.without_ocs_header.push(route.to_string());
        }
        let params: HashMap<String, Json> = if req.method == "GET" {
            query(q).into_iter().map(|(k, v)| (k, v.parse::<u64>().map(Json::from).unwrap_or(Json::String(v)))).collect()
        } else {
            serde_json::from_slice(&req.body).unwrap_or_default()
        };
        return collab(d, route, &params);
    }
    json_answer(404, json!({}))
}

fn webdav(d: &mut Data, req: &Request, path: &str) -> Answer {
    let etag_headers = |etag: &str| vec![("ETag".to_string(), format!("\"{etag}\"")), ("OC-ETag".to_string(), format!("\"{etag}\""))];
    match req.method.as_str() {
        "GET" | "HEAD" => match d.files.get(path) {
            Some(f) => (200, etag_headers(&f.etag), f.bytes.clone()),
            None => (404, vec![], vec![]),
        },
        "PUT" => {
            let current = d.files.get(path).map(|f| f.etag.clone());
            let wanted = req.headers.get("if-match").map(|e| e.trim_matches('"').to_string());
            let none = req.headers.get("if-none-match").is_some_and(|v| v == "*");
            if (none && current.is_some()) || wanted.is_some_and(|w| Some(w) != current) {
                return (412, vec![], vec![]);
            }
            let etag = d.put(path, req.body.clone());
            (if current.is_some() { 204 } else { 201 }, etag_headers(&etag), vec![])
        }
        "MKCOL" => (201, vec![], vec![]),
        "PROPFIND" => {
            let depth = req.headers.get("depth").cloned().unwrap_or_default();
            let mut xml = String::from(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">"#);
            let href = |p: &str| format!("/remote.php/dav/files/{USER}/{}", p.split('/').map(super::encode).collect::<Vec<_>>().join("/"));
            if depth == "0" {
                let Some(f) = d.files.get(path) else { return (404, vec![], vec![]) };
                xml.push_str(&format!(
                    "<d:response><d:href>{}</d:href><d:propstat><d:prop><oc:fileid>{}</oc:fileid><oc:permissions>RGDNVW</oc:permissions></d:prop></d:propstat></d:response>",
                    href(path),
                    f.id
                ));
            } else {
                let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
                xml.push_str(&format!("<d:response><d:href>{}/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>", href(path)));
                let mut folders = Vec::new();
                for (p, f) in &d.files {
                    let Some(rest) = p.strip_prefix(&prefix) else { continue };
                    match rest.split_once('/') {
                        Some((folder, _)) => {
                            if !folders.contains(&folder.to_string()) {
                                folders.push(folder.to_string());
                            }
                        }
                        None => xml.push_str(&format!(
                            "<d:response><d:href>{}</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>{}</d:getcontentlength></d:prop></d:propstat></d:response>",
                            href(p),
                            f.bytes.len()
                        )),
                    }
                }
                for folder in folders {
                    xml.push_str(&format!(
                        "<d:response><d:href>{}/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>",
                        href(&format!("{prefix}{folder}"))
                    ));
                }
            }
            xml.push_str("</d:multistatus>");
            (207, vec![("Content-Type".into(), "application/xml".into())], xml.into_bytes())
        }
        _ => (405, vec![], vec![]),
    }
}

fn collab(d: &mut Data, route: &str, p: &HashMap<String, Json>) -> Answer {
    let num = |k: &str| p.get(k).and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok())).unwrap_or(0);
    let text = |k: &str| p.get(k).and_then(Json::as_str).unwrap_or("").to_string();
    let entries = |s: &Session, since: u64| -> Json {
        s.log
            .iter()
            .filter(|e| e.0 > since)
            .map(|(seq, client, cs, base, op)| json!({"seq": seq, "client": client, "client_seq": cs, "base": base, "op": op}))
            .collect()
    };
    let members = |s: &Session| -> Json {
        s.members
            .iter()
            .filter(|m| m.4)
            .map(|(client, name, sel, edit, _)| json!({"client": client, "userId": name.to_lowercase(), "displayName": name, "selection": sel, "canEdit": edit}))
            .collect()
    };
    match route {
        "join" => {
            let (file_id, etag) = (num("fileId"), text("etag"));
            let s =
                d.sessions.entry(file_id).or_insert_with(|| Session { file_id, checkpoint_etag: etag.clone(), next_client: 1, ..Session::default() });
            if s.checkpoint_etag != etag {
                if s.members.iter().any(|m| m.4) {
                    return json_answer(200, json!({"retry": true}));
                }
                // Nobody's here and the file changed: the log starts again from it.
                s.checkpoint_seq = s.log.last().map_or(0, |e| e.0);
                s.checkpoint_etag = etag;
                s.snapshot = None;
            }
            if text("build").parse::<u32>().ok() > s.build.parse::<u32>().ok() {
                s.build = text("build");
            }
            s.next_client += 1;
            let client = s.next_client;
            s.builds.push((client, text("build")));
            s.members.push((client, "Desktop".into(), String::new(), true, true));
            let seq = s.checkpoint_seq;
            let has_state = seq > 0 && s.snapshot.as_ref().is_some_and(|(at, _)| *at == seq);
            json_answer(
                200,
                json!({"sessionId": file_id, "client": client, "seq": seq, "hasState": has_state, "entries": entries(s, seq), "members": members(s), "canEdit": true, "build": s.build}),
            )
        }
        "state" => {
            let Some(s) = d.sessions.get(&num("sessionId")) else { return json_answer(409, json!({"rejoin": true})) };
            match &s.snapshot {
                Some((at, state)) if *at == num("seq") => (200, vec![("Content-Type".into(), "application/json".into())], state.clone().into_bytes()),
                _ => json_answer(404, json!({"retry": true})),
            }
        }
        "push" => {
            let Some(s) = d.sessions.get_mut(&num("sessionId")) else { return json_answer(409, json!({"rejoin": true})) };
            let (client, cs) = (num("client"), num("clientSeq"));
            if s.outdated(client) {
                return json_answer(409, json!({"rejoin": true, "outdated": true}));
            }
            if let Some(e) = s.log.iter().find(|e| e.1 == client && e.2 == cs) {
                return json_answer(200, json!({"seq": e.0}));
            }
            let seq = s.log.last().map_or(s.checkpoint_seq, |e| e.0) + 1;
            s.log.push((seq, client, cs, num("base"), text("op")));
            json_answer(200, json!({"seq": seq}))
        }
        "sync" => {
            let Some(s) = d.sessions.get_mut(&num("sessionId")) else { return json_answer(409, json!({"rejoin": true})) };
            let (client, sel) = (num("client"), text("sel"));
            if s.outdated(client) {
                return json_answer(409, json!({"rejoin": true, "outdated": true}));
            }
            if let Some(m) = s.members.iter_mut().find(|m| m.0 == client)
                && !sel.is_empty()
            {
                m.2 = sel;
            }
            json_answer(
                200,
                json!({"entries": entries(s, num("since")), "members": members(s), "checkpointSeq": s.checkpoint_seq, "checkpointEtag": s.checkpoint_etag}),
            )
        }
        "checkpoint" => {
            let sid = num("sessionId");
            let etag = text("etag");
            let Some(file_id) = d.sessions.get(&sid).map(|s| s.file_id) else { return json_answer(409, json!({"rejoin": true})) };
            let saved = d.files.values().find(|f| f.id == file_id).map(|f| f.etag.clone());
            if saved != Some(etag.clone()) {
                return json_answer(400, json!({"error": "the file has changed since that save"}));
            }
            let Some(s) = d.sessions.get_mut(&sid) else { return json_answer(409, json!({"rejoin": true})) };
            let seq = num("seq");
            s.checkpoints.push((num("client"), seq, etag.clone(), text("state")));
            if seq > s.checkpoint_seq {
                s.checkpoint_seq = seq;
                s.checkpoint_etag = etag;
                let state = text("state");
                s.snapshot = (!state.is_empty()).then_some((seq, state));
            }
            json_answer(200, json!([]))
        }
        "leave" => {
            if let Some(s) = d.sessions.get_mut(&num("sessionId"))
                && let Some(m) = s.members.iter_mut().find(|m| m.0 == num("client"))
            {
                m.4 = false;
            }
            json_answer(200, json!([]))
        }
        _ => json_answer(404, json!({})),
    }
}
