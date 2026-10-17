//! Everything in a document besides its stories, as keyed values: settings and page setup by
//! setting, styles by style id, lists by id, comments by id, pictures by media key… An edit sends
//! the keys it changed; two people changing different keys both keep theirs, and the same key,
//! the later change wins.

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine as _;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value as Json;
use wordcraft_doc::Document;

use super::delta::{Map, diff_map, patch_map};
use super::{Error, Result};

/// Changed keys by field (`null`: the key went).
pub type Fields = BTreeMap<String, Map>;

/// Settings that are each person's own: never shared.
const LOCAL_SETTINGS: &[&str] = &["zoom"];

fn err(e: impl std::fmt::Display) -> Error {
    Error(format!("co-editing: {e}"))
}

fn to_map<T: Serialize>(v: &T) -> Result<Map> {
    match serde_json::to_value(v).map_err(err)? {
        Json::Object(m) => Ok(m),
        _ => Ok(Map::new()),
    }
}

fn from_json<T: DeserializeOwned>(v: Json) -> Result<T> {
    serde_json::from_value(v).map_err(|e| Error(format!("co-editing: bad setting in an edit: {e}")))
}

/// A struct as shallow keys.
fn struct_diff<T: Serialize + PartialEq>(a: &T, b: &T, skip: &[&str]) -> Result<Map> {
    if a == b {
        return Ok(Map::new());
    }
    let mut p = diff_map(&to_map(a)?, &to_map(b)?);
    p.retain(|k, _| !skip.contains(&k.as_str()));
    Ok(p)
}

fn struct_patch<T: Serialize + DeserializeOwned>(t: &mut T, p: &Map, skip: &[&str]) -> Result<()> {
    let mut m = to_map(t)?;
    let p: Map = p.iter().filter(|(k, _)| !skip.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    patch_map(&mut m, &p);
    *t = from_json(Json::Object(m))?;
    Ok(())
}

fn struct_get<T: Serialize>(t: &T, key: &str) -> Result<Json> {
    Ok(to_map(t)?.remove(key).unwrap_or(Json::Null))
}

/// A list of things with keys, as keys.
fn list_diff<T: Serialize + PartialEq>(a: &[T], b: &[T], key: impl Fn(&T) -> String) -> Result<Map> {
    let mut p = Map::new();
    if a == b {
        return Ok(p);
    }
    let old: BTreeMap<String, &T> = a.iter().rev().map(|t| (key(t), t)).collect();
    let mut seen = std::collections::BTreeSet::new();
    for t in b {
        let k = key(t);
        if !seen.insert(k.clone()) {
            continue;
        }
        if old.get(&k).is_none_or(|o| *o != t) {
            p.insert(k, serde_json::to_value(t).map_err(err)?);
        }
    }
    for k in old.keys() {
        if !seen.contains(k) {
            p.insert(k.clone(), Json::Null);
        }
    }
    Ok(p)
}

fn list_set<T: DeserializeOwned>(list: &mut Vec<T>, key: impl Fn(&T) -> String, k: &str, v: &Json) -> Result<()> {
    let at = list.iter().position(|t| key(t) == k);
    match (at, v.is_null()) {
        (Some(i), true) => {
            list.remove(i);
        }
        (None, true) => {}
        (Some(i), false) => {
            if let Some(slot) = list.get_mut(i) {
                *slot = from_json(v.clone())?;
            }
        }
        (None, false) => list.push(from_json(v.clone())?),
    }
    Ok(())
}

fn list_get<T: Serialize>(list: &[T], key: impl Fn(&T) -> String, k: &str) -> Result<Json> {
    match list.iter().find(|t| key(t) == k) {
        Some(t) => serde_json::to_value(t).map_err(err),
        None => Ok(Json::Null),
    }
}

/// A map by number, as keys.
fn idmap_diff<T: Serialize + PartialEq>(a: &BTreeMap<u32, T>, b: &BTreeMap<u32, T>) -> Result<Map> {
    let mut p = Map::new();
    if a == b {
        return Ok(p);
    }
    for (k, v) in b {
        if a.get(k) != Some(v) {
            p.insert(k.to_string(), serde_json::to_value(v).map_err(err)?);
        }
    }
    for k in a.keys().filter(|k| !b.contains_key(k)) {
        p.insert(k.to_string(), Json::Null);
    }
    Ok(p)
}

fn idmap_set<T: DeserializeOwned>(m: &mut BTreeMap<u32, T>, k: &str, v: &Json) -> Result<()> {
    let id: u32 = k.parse().map_err(|_| Error(format!("co-editing: bad id {k:?} in an edit")))?;
    if v.is_null() {
        m.remove(&id);
    } else {
        m.insert(id, from_json(v.clone())?);
    }
    Ok(())
}

fn idmap_get<T: Serialize>(m: &BTreeMap<u32, T>, k: &str) -> Result<Json> {
    match k.parse::<u32>().ok().and_then(|id| m.get(&id)) {
        Some(v) => serde_json::to_value(v).map_err(err),
        None => Ok(Json::Null),
    }
}

/// Bytes by name (pictures, kept parts), as base64.
fn bytes_diff(a: &BTreeMap<String, Arc<Vec<u8>>>, b: &BTreeMap<String, Arc<Vec<u8>>>) -> Map {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut p = Map::new();
    for (k, v) in b {
        if a.get(k).is_none_or(|o| !Arc::ptr_eq(o, v) && o != v) {
            p.insert(k.clone(), Json::String(b64.encode(v.as_slice())));
        }
    }
    for k in a.keys().filter(|k| !b.contains_key(*k)) {
        p.insert(k.clone(), Json::Null);
    }
    p
}

fn bytes_set(m: &mut BTreeMap<String, Arc<Vec<u8>>>, k: &str, v: &Json) -> Result<()> {
    match v {
        Json::Null => {
            m.remove(k);
        }
        Json::String(s) => {
            let bytes = base64::engine::general_purpose::STANDARD.decode(s).map_err(|_| Error("co-editing: bad picture data in an edit".into()))?;
            m.insert(k.to_string(), Arc::new(bytes));
        }
        _ => return Err(Error("co-editing: bad picture data in an edit".into())),
    }
    Ok(())
}

fn bytes_get(m: &BTreeMap<String, Arc<Vec<u8>>>, k: &str) -> Json {
    m.get(k).map_or(Json::Null, |v| Json::String(base64::engine::general_purpose::STANDARD.encode(v.as_slice())))
}

fn style_key(s: &wordcraft_doc::Style) -> String {
    s.id.clone()
}
fn abstract_key(a: &wordcraft_doc::numbering::AbstractNum) -> String {
    a.id.to_string()
}
fn num_key(n: &wordcraft_doc::numbering::Num) -> String {
    n.id.to_string()
}
fn source_key(s: &wordcraft_doc::Source) -> String {
    s.tag.clone()
}
fn custom_key(c: &wordcraft_doc::CustomProp) -> String {
    c.name.clone()
}

/// A part's own settings (its stories travel as edits of their own).
#[derive(serde::Serialize, serde::Deserialize, PartialEq)]
struct PartMeta {
    kind: wordcraft_doc::PartKind,
    #[serde(default)]
    body: wordcraft_doc::TextBody,
}

fn part_meta(p: &wordcraft_doc::Part) -> PartMeta {
    PartMeta { kind: p.kind, body: p.body }
}

/// What changed between two documents, stories aside.
pub(crate) fn diff_fields(a: &Document, b: &Document) -> Result<Fields> {
    let mut f = Fields::new();
    let mut put = |name: &str, p: Map| {
        if !p.is_empty() {
            f.insert(name.to_string(), p);
        }
    };
    put("settings", struct_diff(&a.settings, &b.settings, LOCAL_SETTINGS)?);
    put("core", struct_diff(&a.core, &b.core, &[])?);
    put("lastSection", struct_diff(&a.last_section, &b.last_section, &[])?);
    put("footnoteSeparators", struct_diff(&a.footnote_separators, &b.footnote_separators, &[])?);
    put("endnoteSeparators", struct_diff(&a.endnote_separators, &b.endnote_separators, &[])?);
    if a.styles != b.styles {
        put("styles", struct_diff(&a.styles, &b.styles, &["styles"])?);
        put("styles.list", list_diff(&a.styles.styles, &b.styles.styles, style_key)?);
    }
    put("numbering.abstracts", list_diff(&a.numbering.abstracts, &b.numbering.abstracts, abstract_key)?);
    put("numbering.nums", list_diff(&a.numbering.nums, &b.numbering.nums, num_key)?);
    put("comments", idmap_diff(&a.comments, &b.comments)?);
    put("sources", list_diff(&a.sources, &b.sources, source_key)?);
    put("customProps", list_diff(&a.custom_props, &b.custom_props, custom_key)?);
    put("media", bytes_diff(&a.media, &b.media));
    put("passthrough", bytes_diff(&a.passthrough, &b.passthrough));
    let mut meta = Map::new();
    for (id, pb) in &b.parts {
        if let Some(pa) = a.parts.get(id)
            && (pa.kind != pb.kind || pa.body != pb.body)
        {
            meta.insert(id.to_string(), serde_json::to_value(part_meta(pb)).map_err(err)?);
        }
    }
    put("partMeta", meta);
    Ok(f)
}

/// Sets one key.
fn set(d: &mut Document, field: &str, k: &str, v: &Json) -> Result<()> {
    let one = |k: &str, v: &Json| -> Map { std::iter::once((k.to_string(), v.clone())).collect() };
    match field {
        "settings" => struct_patch(&mut d.settings, &one(k, v), LOCAL_SETTINGS),
        "core" => struct_patch(&mut d.core, &one(k, v), &[]),
        "lastSection" => struct_patch(&mut d.last_section, &one(k, v), &[]),
        "footnoteSeparators" => struct_patch(&mut d.footnote_separators, &one(k, v), &[]),
        "endnoteSeparators" => struct_patch(&mut d.endnote_separators, &one(k, v), &[]),
        "styles" => struct_patch(&mut d.styles, &one(k, v), &["styles"]),
        "styles.list" => list_set(&mut d.styles.styles, style_key, k, v),
        "numbering.abstracts" => list_set(&mut d.numbering.abstracts, abstract_key, k, v),
        "numbering.nums" => list_set(&mut d.numbering.nums, num_key, k, v),
        "comments" => idmap_set(&mut d.comments, k, v),
        "sources" => list_set(&mut d.sources, source_key, k, v),
        "customProps" => list_set(&mut d.custom_props, custom_key, k, v),
        "media" => bytes_set(&mut d.media, k, v),
        "passthrough" => bytes_set(&mut d.passthrough, k, v),
        "partMeta" => {
            // A part someone else removed meanwhile: nothing to set.
            if let Some(p) = k.parse::<u32>().ok().and_then(|id| d.parts.get_mut(&id))
                && !v.is_null()
            {
                let m: PartMeta = from_json(v.clone())?;
                p.kind = m.kind;
                p.body = m.body;
            }
            Ok(())
        }
        other => Err(Error(format!("co-editing: unknown setting {other:?} in an edit"))),
    }
}

/// One key's value now (`null`: none).
fn get(d: &Document, field: &str, k: &str) -> Result<Json> {
    match field {
        "settings" => struct_get(&d.settings, k),
        "core" => struct_get(&d.core, k),
        "lastSection" => struct_get(&d.last_section, k),
        "footnoteSeparators" => struct_get(&d.footnote_separators, k),
        "endnoteSeparators" => struct_get(&d.endnote_separators, k),
        "styles" => struct_get(&d.styles, k),
        "styles.list" => list_get(&d.styles.styles, style_key, k),
        "numbering.abstracts" => list_get(&d.numbering.abstracts, abstract_key, k),
        "numbering.nums" => list_get(&d.numbering.nums, num_key, k),
        "comments" => idmap_get(&d.comments, k),
        "sources" => list_get(&d.sources, source_key, k),
        "customProps" => list_get(&d.custom_props, custom_key, k),
        "media" => Ok(bytes_get(&d.media, k)),
        "passthrough" => Ok(bytes_get(&d.passthrough, k)),
        "partMeta" => match k.parse::<u32>().ok().and_then(|id| d.parts.get(&id)) {
            Some(p) => serde_json::to_value(part_meta(p)).map_err(err),
            None => Ok(Json::Null),
        },
        other => Err(Error(format!("co-editing: unknown setting {other:?} in an edit"))),
    }
}

pub(crate) fn apply_fields(d: &mut Document, f: &Fields) -> Result<()> {
    for (field, keys) in f {
        if field == "styles" {
            // Several keys of one struct at once (one round trip through JSON).
            struct_patch(&mut d.styles, keys, &["styles"])?;
            continue;
        }
        for (k, v) in keys {
            set(d, field, k, v)?;
        }
    }
    Ok(())
}

/// The keys `f` changes, as they are in `d` (to take `f` back).
pub(crate) fn invert_fields(d: &Document, f: &Fields) -> Result<Fields> {
    let mut out = Fields::new();
    for (field, keys) in f {
        let mut m = Map::new();
        for k in keys.keys() {
            m.insert(k.clone(), get(d, field, k)?);
        }
        out.insert(field.clone(), m);
    }
    Ok(out)
}

/// `a` rebased after `b`: where `a` comes first, `b`'s keys win.
pub(crate) fn transform_fields(a: &Fields, b: &Fields, a_first: bool) -> Fields {
    if !a_first {
        return a.clone();
    }
    let mut out = Fields::new();
    for (field, keys) in a {
        let theirs = b.get(field);
        let m: Map = keys.iter().filter(|(k, _)| theirs.is_none_or(|t| !t.contains_key(*k))).map(|(k, v)| (k.clone(), v.clone())).collect();
        if !m.is_empty() {
            out.insert(field.clone(), m);
        }
    }
    out
}

/// `a` then `b`.
pub(crate) fn compose_fields(a: &Fields, b: &Fields) -> Fields {
    let mut out = a.clone();
    for (field, keys) in b {
        let m = out.entry(field.clone()).or_default();
        for (k, v) in keys {
            m.insert(k.clone(), v.clone());
        }
    }
    out
}

/// Every JSON payload in `f`, for renumbering revisions. Pictures and kept parts hold none.
pub(crate) fn payloads_mut(f: &mut Fields) -> impl Iterator<Item = &mut Map> {
    f.iter_mut().filter(|(k, _)| k.as_str() != "media" && k.as_str() != "passthrough").map(|(_, m)| m)
}
