use std::collections::HashMap;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::metadata::FileMeta;
use crate::template::Template;

#[derive(Debug, Clone)]
pub struct Settings {
    /// Folder scanned for files to rename.
    pub source: PathBuf,
    /// Root the rendered relative paths are placed under. Defaults to `source`.
    pub output_root: PathBuf,
    /// Template string, e.g. `{date_taken:%Y}/{date_taken:%m}/{name}`.
    pub template: String,
    /// Recurse into subdirectories of `source`.
    pub recursive: bool,
    /// Append the original extension if the rendered name lacks one.
    pub append_ext: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RowStatus {
    Ok,
    /// Two or more files render to the same target.
    Collision,
    /// Target already exists on disk and isn't one of our sources.
    Exists,
    /// Template field missing for this file.
    Error(String),
    /// Rename happened.
    Done,
    /// Rename failed at apply time.
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct PlanRow {
    pub src: PathBuf,
    /// Rendered relative target path (under `output_root`). Empty on Error.
    pub rel_target: String,
    pub status: RowStatus,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub rows: Vec<PlanRow>,
    pub output_root: PathBuf,
}

impl Plan {
    pub fn ok_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.status == RowStatus::Ok)
            .count()
    }
    pub fn problem_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| !matches!(r.status, RowStatus::Ok | RowStatus::Done))
            .count()
    }
}

/// Collect candidate files from `source`.
fn collect_files(source: &Path, recursive: bool) -> Vec<PathBuf> {
    let walker = WalkDir::new(source).min_depth(1);
    let walker = if recursive {
        walker
    } else {
        walker.max_depth(1)
    };
    let mut files: Vec<PathBuf> = walker
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect();
    // Sort so the {n} counter is stable across runs.
    files.sort();
    files
}

/// Build a preview plan without touching the filesystem.
pub fn build_plan(settings: &Settings) -> Plan {
    let template = Template::parse(&settings.template);
    let files = collect_files(&settings.source, settings.recursive);

    let mut rows: Vec<PlanRow> = Vec::with_capacity(files.len());
    // target rel path -> indices of rows producing it (for collision detection)
    let mut targets: HashMap<String, Vec<usize>> = HashMap::new();

    for (i, src) in files.into_iter().enumerate() {
        let meta = FileMeta::read(&src);
        let (rel_target, status) = match template.render(&meta, i + 1) {
            Ok(mut rel) => {
                if settings.append_ext && !meta.ext.is_empty() {
                    let suffix = format!(".{}", meta.ext.to_lowercase());
                    if !rel.to_lowercase().ends_with(&suffix) {
                        rel.push('.');
                        rel.push_str(&meta.ext);
                    }
                }
                (rel, RowStatus::Ok)
            }
            Err(e) => (String::new(), RowStatus::Error(e.to_string())),
        };

        if matches!(status, RowStatus::Ok) {
            targets.entry(rel_target.clone()).or_default().push(i);
        }
        rows.push(PlanRow {
            src,
            rel_target,
            status,
        });
    }

    // Mark collisions (same rendered target across multiple files).
    for idxs in targets.values() {
        if idxs.len() > 1 {
            for &i in idxs {
                rows[i].status = RowStatus::Collision;
            }
        }
    }

    // Mark targets that already exist on disk and aren't a source we're moving.
    let sources: std::collections::HashSet<PathBuf> =
        rows.iter().map(|r| r.src.clone()).collect();
    for row in rows.iter_mut() {
        if row.status == RowStatus::Ok {
            let abs = settings.output_root.join(&row.rel_target);
            if abs.exists() && abs != row.src && !sources.contains(&abs) {
                row.status = RowStatus::Exists;
            }
        }
    }

    Plan {
        rows,
        output_root: settings.output_root.clone(),
    }
}

/// Apply every `Ok` row in the plan: create parent folders, then rename.
/// Mutates row statuses to `Done`/`Failed`. Returns count renamed.
pub fn apply_plan(plan: &mut Plan) -> usize {
    let mut done = 0;
    for row in plan.rows.iter_mut() {
        if row.status != RowStatus::Ok {
            continue;
        }
        let dest = plan.output_root.join(&row.rel_target);
        if let Some(parent) = dest.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                row.status = RowStatus::Failed(format!("mkdir: {e}"));
                continue;
            }
        }
        match std::fs::rename(&row.src, &dest) {
            Ok(_) => {
                row.status = RowStatus::Done;
                done += 1;
            }
            Err(e) => row.status = RowStatus::Failed(e.to_string()),
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("renamer_test_{}_{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn plan_and_apply_creates_folders() {
        let dir = tmpdir("apply");
        fs::write(dir.join("a.txt"), "1").unwrap();
        fs::write(dir.join("b.txt"), "2").unwrap();

        let s = Settings {
            source: dir.clone(),
            output_root: dir.clone(),
            template: "sorted/{n:02}_{name}".into(),
            recursive: false,
            append_ext: true,
        };
        let mut plan = build_plan(&s);
        assert_eq!(plan.rows.len(), 2);
        assert_eq!(plan.ok_count(), 2);

        let n = apply_plan(&mut plan);
        assert_eq!(n, 2);
        // folder created, files moved with extension kept
        let moved: Vec<_> = fs::read_dir(dir.join("sorted"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(moved.len(), 2);
        assert!(moved.iter().all(|f| f.ends_with(".txt")));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn detects_collision() {
        let dir = tmpdir("collision");
        fs::write(dir.join("a.txt"), "1").unwrap();
        fs::write(dir.join("b.txt"), "2").unwrap();
        let s = Settings {
            source: dir.clone(),
            output_root: dir.clone(),
            template: "same".into(), // both render to "same.txt"
            recursive: false,
            append_ext: true,
        };
        let plan = build_plan(&s);
        assert!(plan.rows.iter().all(|r| r.status == RowStatus::Collision));
        fs::remove_dir_all(&dir).unwrap();
    }
}
