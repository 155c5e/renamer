//! Minimal pCloud HTTP API client + server-side rename planning.
//!
//! Operating on pCloud over its API means metadata and renames happen on
//! pCloud's servers — no file contents are ever downloaded, which is the whole
//! point for large folders streamed from the cloud. EXIF (date_taken, camera,
//! …) is *not* exposed by the API, so in this mode those template fields are
//! unavailable; use the filesystem-style `date_modified` / `date_created`
//! instead.

use chrono::{Local, TimeZone};
use serde_json::Value;

use crate::engine::RowStatus;
use crate::metadata::FileMeta;
use crate::template::Template;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// api.pcloud.com — accounts created in the US data region.
    Us,
    /// eapi.pcloud.com — accounts created in the EU data region.
    Eu,
}

impl Region {
    fn base(self) -> &'static str {
        match self {
            Region::Us => "https://api.pcloud.com",
            Region::Eu => "https://eapi.pcloud.com",
        }
    }
}

/// Authenticated client. Cheap to clone (shares the underlying agent + token).
#[derive(Clone)]
pub struct Client {
    base: &'static str,
    auth: String,
    agent: ureq::Agent,
}

/// One remote file discovered by a scan.
#[derive(Debug, Clone)]
pub struct RemoteFile {
    pub fileid: u64,
    /// Absolute pCloud path, e.g. `/Camera/IMG_0001.jpg`.
    pub path: String,
    pub meta: FileMeta,
}

/// A planned server-side rename, kept alongside the display plan so apply/undo
/// can reach the file by id.
#[derive(Debug, Clone)]
pub struct RemoteRow {
    pub fileid: u64,
    pub from_path: String,
    pub display_name: String,
    pub rel_target: String,
    pub status: RowStatus,
}

#[derive(Debug, Clone)]
pub struct RemotePlan {
    pub rows: Vec<RemoteRow>,
    /// Absolute base path renders are placed under.
    pub base: String,
}

impl RemotePlan {
    pub fn ok_count(&self) -> usize {
        self.rows.iter().filter(|r| r.status == RowStatus::Ok).count()
    }
    pub fn problem_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| !matches!(r.status, RowStatus::Ok | RowStatus::Done))
            .count()
    }
}

/// A completed server-side move, for undo.
#[derive(Debug, Clone)]
pub struct RemoteUndo {
    pub fileid: u64,
    pub from_path: String,
    pub to_path: String,
}

impl Client {
    /// Authenticate with email + password and obtain an auth token.
    pub fn login(region: Region, username: &str, password: &str) -> Result<Client, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(30))
            .build();
        let base = region.base();
        let resp = agent
            .post(&format!("{base}/userinfo"))
            .send_form(&[
                ("getauth", "1"),
                ("logout", "0"),
                ("username", username),
                ("password", password),
            ])
            .map_err(|e| format!("network error: {e}"))?;
        let v: Value = resp.into_json().map_err(|e| format!("bad response: {e}"))?;
        check_result(&v)?;
        let auth = v["auth"]
            .as_str()
            .ok_or("login succeeded but no auth token returned")?
            .to_string();
        Ok(Client {
            base,
            auth,
            agent,
        })
    }

    fn get(&self, method: &str, params: &[(&str, &str)]) -> Result<Value, String> {
        let mut req = self
            .agent
            .get(&format!("{}/{}", self.base, method))
            .query("auth", &self.auth)
            .query("timeformat", "timestamp");
        for (k, v) in params {
            req = req.query(k, v);
        }
        let resp = req.call().map_err(|e| format!("network error: {e}"))?;
        let v: Value = resp.into_json().map_err(|e| format!("bad response: {e}"))?;
        check_result(&v)?;
        Ok(v)
    }

    /// List files under `path`. With `recursive`, descends into subfolders.
    pub fn listfolder(&self, path: &str, recursive: bool) -> Result<Vec<RemoteFile>, String> {
        let path = normalize_dir(path);
        let v = self.get(
            "listfolder",
            &[("path", &path), ("recursive", if recursive { "1" } else { "0" })],
        )?;
        let mut out = Vec::new();
        if let Some(contents) = v["metadata"]["contents"].as_array() {
            collect(contents, &path, &mut out);
        }
        Ok(out)
    }

    /// Create `path` and any missing parent folders.
    pub fn create_folder_all(&self, path: &str) -> Result<(), String> {
        // Build up each ancestor: /a, /a/b, /a/b/c
        let mut acc = String::new();
        for comp in path.trim_matches('/').split('/').filter(|c| !c.is_empty()) {
            acc.push('/');
            acc.push_str(comp);
            self.get("createfolderifnotexists", &[("path", &acc)])?;
        }
        Ok(())
    }

    /// Move/rename a file to an absolute destination path.
    pub fn rename_to_path(&self, fileid: u64, to_path: &str) -> Result<(), String> {
        self.get(
            "renamefile",
            &[("fileid", &fileid.to_string()), ("topath", to_path)],
        )?;
        Ok(())
    }
}

/// Check pCloud's `result` field; non-zero is an error carrying `error` text.
fn check_result(v: &Value) -> Result<(), String> {
    match v["result"].as_i64() {
        Some(0) => Ok(()),
        Some(code) => {
            let msg = v["error"].as_str().unwrap_or("unknown error");
            Err(format!("pCloud error {code}: {msg}"))
        }
        None => Err("malformed pCloud response (no result field)".to_string()),
    }
}

fn normalize_dir(path: &str) -> String {
    let p = path.trim();
    if p.is_empty() || p == "/" {
        "/".to_string()
    } else {
        format!("/{}", p.trim_matches('/'))
    }
}

/// Recursively flatten pCloud `contents` arrays into files with absolute paths.
fn collect(contents: &[Value], dir: &str, out: &mut Vec<RemoteFile>) {
    for item in contents {
        let name = item["name"].as_str().unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let child_path = if dir == "/" {
            format!("/{name}")
        } else {
            format!("{dir}/{name}")
        };
        if item["isfolder"].as_bool().unwrap_or(false) {
            if let Some(sub) = item["contents"].as_array() {
                collect(sub, &child_path, out);
            }
        } else if let Some(fileid) = item["fileid"].as_u64() {
            out.push(RemoteFile {
                fileid,
                meta: meta_from_item(item, &name, dir),
                path: child_path,
            });
        }
    }
}

/// Build a `FileMeta` from pCloud metadata. EXIF-only fields stay `None`.
fn meta_from_item(item: &Value, name: &str, dir: &str) -> FileMeta {
    let (stem, ext) = split_name(name);
    let mut m = FileMeta {
        name: stem,
        ext,
        parent: dir.rsplit('/').next().unwrap_or("").to_string(),
        size: item["size"].as_u64().unwrap_or(0),
        ..FileMeta::default()
    };
    let ts = |key: &str| {
        item[key]
            .as_i64()
            .and_then(|s| Local.timestamp_opt(s, 0).single())
    };
    m.date_created = ts("created");
    m.date_modified = ts("modified");
    if let Some(w) = item["width"].as_u64() {
        m.width = Some(w.to_string());
    }
    if let Some(h) = item["height"].as_u64() {
        m.height = Some(h.to_string());
    }
    m
}

fn split_name(name: &str) -> (String, String) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), ext.to_string()),
        _ => (name.to_string(), String::new()),
    }
}

/// Build a rename plan for remote files (collision detection, no exists check
/// — that would cost extra API calls; collisions still catch dup targets).
pub fn build_remote_plan(
    files: &[RemoteFile],
    base: &str,
    template_str: &str,
    append_ext: bool,
) -> RemotePlan {
    let template = Template::parse(template_str);
    let mut rows: Vec<RemoteRow> = Vec::with_capacity(files.len());
    let mut targets: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();

    for (i, f) in files.iter().enumerate() {
        let (rel_target, status) = match template.render(&f.meta, i + 1) {
            Ok(mut rel) => {
                if append_ext && !f.meta.ext.is_empty() {
                    let suffix = format!(".{}", f.meta.ext.to_lowercase());
                    if !rel.to_lowercase().ends_with(&suffix) {
                        rel.push('.');
                        rel.push_str(&f.meta.ext);
                    }
                }
                (rel, RowStatus::Ok)
            }
            Err(e) => (String::new(), RowStatus::Error(e.to_string())),
        };
        if status == RowStatus::Ok {
            targets.entry(rel_target.clone()).or_default().push(i);
        }
        rows.push(RemoteRow {
            fileid: f.fileid,
            from_path: f.path.clone(),
            display_name: f
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&f.path)
                .to_string(),
            rel_target,
            status,
        });
    }

    for idxs in targets.values() {
        if idxs.len() > 1 {
            for &i in idxs {
                rows[i].status = RowStatus::Collision;
            }
        }
    }

    RemotePlan {
        rows,
        base: normalize_dir(base),
    }
}

/// Apply every `Ok` row server-side. Mutates statuses; returns the undo log.
pub fn apply_remote_plan(
    client: &Client,
    plan: &mut RemotePlan,
    progress: &crate::engine::Progress,
) -> Vec<RemoteUndo> {
    let base = plan.base.trim_end_matches('/').to_string();
    let total = plan.rows.iter().filter(|r| r.status == RowStatus::Ok).count();
    progress.set_total(total);

    let mut undo = Vec::new();
    for row in plan.rows.iter_mut() {
        if row.status != RowStatus::Ok {
            continue;
        }
        progress.inc();
        let to_path = format!("{base}/{}", row.rel_target.trim_start_matches('/'));
        // Create parent folders if the target lives in a subfolder.
        if let Some((parent, _)) = to_path.rsplit_once('/') {
            if !parent.is_empty() {
                if let Err(e) = client.create_folder_all(parent) {
                    row.status = RowStatus::Failed(format!("mkdir: {e}"));
                    continue;
                }
            }
        }
        match client.rename_to_path(row.fileid, &to_path) {
            Ok(_) => {
                row.status = RowStatus::Done;
                undo.push(RemoteUndo {
                    fileid: row.fileid,
                    from_path: row.from_path.clone(),
                    to_path,
                });
            }
            Err(e) => row.status = RowStatus::Failed(e),
        }
    }
    undo
}

/// Reverse a remote rename batch (newest first). Returns `(reverted, errors)`.
pub fn undo_remote(client: &Client, entries: &[RemoteUndo]) -> (usize, Vec<String>) {
    let mut reverted = 0;
    let mut errors = Vec::new();
    for e in entries.iter().rev() {
        match client.rename_to_path(e.fileid, &e.from_path) {
            Ok(_) => reverted += 1,
            Err(err) => errors.push(format!("{}: {err}", e.to_path)),
        }
    }
    (reverted, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_dir_variants() {
        assert_eq!(normalize_dir("/"), "/");
        assert_eq!(normalize_dir(""), "/");
        assert_eq!(normalize_dir("Camera"), "/Camera");
        assert_eq!(normalize_dir("/Camera/"), "/Camera");
        assert_eq!(normalize_dir(" /a/b/ "), "/a/b");
    }

    #[test]
    fn split_name_handles_dotless_and_dotfiles() {
        assert_eq!(split_name("a.jpg"), ("a".into(), "jpg".into()));
        assert_eq!(split_name("README"), ("README".into(), "".into()));
        assert_eq!(split_name(".bashrc"), (".bashrc".into(), "".into()));
    }

    #[test]
    fn meta_from_item_maps_fields_no_exif() {
        let item: Value = serde_json::json!({
            "name": "IMG_1.JPG", "fileid": 9, "isfolder": false,
            "size": 1234, "created": 1_600_000_000_i64, "modified": 1_600_000_500_i64,
            "width": 4000, "height": 3000
        });
        let m = meta_from_item(&item, "IMG_1.JPG", "/Camera");
        assert_eq!(m.name, "IMG_1");
        assert_eq!(m.ext, "JPG");
        assert_eq!(m.parent, "Camera");
        assert_eq!(m.size, 1234);
        assert!(m.date_modified.is_some());
        assert_eq!(m.width.as_deref(), Some("4000"));
        // EXIF-only fields stay empty over the API
        assert!(m.date_taken.is_none());
        assert!(m.camera_model.is_none());
    }

    fn rf(fileid: u64, path: &str, name: &str) -> RemoteFile {
        let (stem, ext) = split_name(name);
        let mut meta = FileMeta::default();
        meta.name = stem;
        meta.ext = ext;
        RemoteFile { fileid, path: path.into(), meta }
    }

    #[test]
    fn remote_plan_renders_and_flags_collisions() {
        let files = vec![
            rf(1, "/Camera/a.jpg", "a.jpg"),
            rf(2, "/Camera/b.jpg", "b.jpg"),
        ];
        // both render to the same name -> collision
        let plan = build_remote_plan(&files, "/Camera", "same", true);
        assert_eq!(plan.base, "/Camera");
        assert!(plan.rows.iter().all(|r| r.status == RowStatus::Collision));
        assert_eq!(plan.ok_count(), 0);

        // distinct targets + subfolder + kept extension
        let plan = build_remote_plan(&files, "/Camera", "sorted/{name}", true);
        assert_eq!(plan.ok_count(), 2);
        assert!(plan.rows[0].rel_target.starts_with("sorted/"));
        assert!(plan.rows[0].rel_target.ends_with(".jpg"));
        assert_eq!(plan.rows[0].fileid, 1);
    }
}
