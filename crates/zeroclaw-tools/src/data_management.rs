use crate::helpers::filesystem_boundary::{
    FilesystemBoundaryError, open_absolute_dir_nofollow, open_dir_nofollow, rename_noreplace,
    rename_noreplace_supported,
};
use async_trait::async_trait;
use cap_fs_ext::MetadataExt;
use cap_std::fs::Dir;
use serde_json::json;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::policy::{SecurityPolicy, ToolOperation};

/// Workspace data lifecycle tool: retention status, time-based purge, and
/// storage statistics.
#[derive(Clone)]
pub struct DataManagementTool {
    retention_days: u64,
    security: Arc<SecurityPolicy>,
}

impl DataManagementTool {
    pub fn new(workspace_dir: PathBuf, retention_days: u64) -> Self {
        let security = Arc::new(SecurityPolicy {
            workspace_dir,
            ..SecurityPolicy::default()
        });
        Self::new_with_security(retention_days, security)
    }

    pub fn new_with_security(retention_days: u64, security: Arc<SecurityPolicy>) -> Self {
        Self {
            retention_days,
            security,
        }
    }

    fn open_workspace(&self) -> anyhow::Result<(PathBuf, Dir)> {
        let canonical = std::fs::canonicalize(&self.security.workspace_dir)?;
        if !self.security.is_resolved_path_readable(&canonical) {
            return Err(data_boundary_violation(tool_text_arg(
                "tool-data-management-error-read-blocked",
                "path",
                &canonical.display().to_string(),
            )));
        }
        Ok((canonical.clone(), open_absolute_dir_nofollow(&canonical)?))
    }

    fn cmd_retention_status(&self) -> anyhow::Result<ToolResult> {
        let cutoff = chrono::Utc::now()
            - chrono::Duration::days(i64::try_from(self.retention_days).unwrap_or(i64::MAX));
        let cutoff_ts = cutoff.timestamp().try_into().unwrap_or(0u64);
        let (_, workspace) = self.open_workspace()?;
        let count = count_files_older_than(&workspace, cutoff_ts)?;

        Ok(ToolResult {
            success: true,
            output: json!({
                "retention_days": self.retention_days,
                "cutoff": cutoff.to_rfc3339(),
                "affected_files": count,
            })
            .to_string()
            .into(),
            error: None,
        })
    }

    fn cmd_purge(&self, dry_run: bool) -> anyhow::Result<ToolResult> {
        if !dry_run
            && self
                .security
                .enforce_tool_operation(ToolOperation::Act, "data retention purge")
                .is_err()
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(crate::i18n::get_required_tool_string(
                    "tool-data-management-error-action-blocked",
                )),
            });
        }
        if !dry_run && !rename_noreplace_supported() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(crate::i18n::get_required_tool_string(
                    "tool-data-management-error-purge-platform",
                )),
            });
        }

        let cutoff = chrono::Utc::now()
            - chrono::Duration::days(i64::try_from(self.retention_days).unwrap_or(i64::MAX));
        let cutoff_ts: u64 = cutoff.timestamp().try_into().unwrap_or(0);
        let (workspace_path, workspace) = self.open_workspace()?;
        if !dry_run && !self.security.is_resolved_path_allowed(&workspace_path) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(tool_text_arg(
                    "tool-data-management-error-write-blocked",
                    "path",
                    &workspace_path.display().to_string(),
                )),
            });
        }
        let workspace = Arc::new(workspace);
        let mut candidates = Vec::new();
        collect_purge_candidates(
            &workspace,
            Path::new(""),
            &workspace_path,
            cutoff_ts,
            &mut candidates,
        )?;
        if !dry_run {
            for candidate in &candidates {
                if !self.security.is_resolved_path_allowed(&candidate.resolved) {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(tool_text_arg(
                            "tool-data-management-error-write-blocked",
                            "path",
                            &candidate.resolved.display().to_string(),
                        )),
                    });
                }
            }
            stage_purge_candidates(&candidates, cutoff_ts)?;
            delete_staged_candidates(&candidates)?;
        }
        let deleted = candidates.len();
        let bytes = candidates
            .iter()
            .map(|candidate| candidate.bytes)
            .sum::<u64>();

        Ok(ToolResult {
            success: true,
            output: json!({
                "dry_run": dry_run,
                "files": deleted,
                "bytes_freed": bytes,
                "bytes_freed_human": format_bytes(bytes),
            })
            .to_string()
            .into(),
            error: None,
        })
    }

    fn cmd_stats(&self) -> anyhow::Result<ToolResult> {
        let (_, workspace) = self.open_workspace()?;
        let (total_files, total_bytes, breakdown) = dir_stats(&workspace)?;
        Ok(ToolResult {
            success: true,
            output: json!({
                "total_files": total_files,
                "total_size": total_bytes,
                "total_size_human": format_bytes(total_bytes),
                "subdirectories": breakdown,
            })
            .to_string()
            .into(),
            error: None,
        })
    }
}

#[async_trait]
impl Tool for DataManagementTool {
    fn name(&self) -> &str {
        "data_management"
    }

    fn description(&self) -> &str {
        "Workspace data retention, purge, and storage statistics"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "enum": ["retention_status", "purge", "stats"],
                    "description": "Data management command"
                },
                "dry_run": {
                    "type": "boolean",
                    "description": "If true, purge only lists what would be deleted (default true)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some("Missing 'command' parameter".into()),
                });
            }
        };

        let result = match command {
            "retention_status" => {
                let tool = self.clone();
                tokio::task::spawn_blocking(move || tool.cmd_retention_status()).await?
            }
            "purge" => {
                let dry_run = args
                    .get("dry_run")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let tool = self.clone();
                tokio::task::spawn_blocking(move || tool.cmd_purge(dry_run)).await?
            }
            "stats" => {
                let tool = self.clone();
                tokio::task::spawn_blocking(move || tool.cmd_stats()).await?
            }
            other => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("Unknown command: {other}")),
            }),
        };

        match result {
            Err(error) if error.downcast_ref::<DataBoundaryViolation>().is_some() => {
                Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(error.to_string()),
                })
            }
            Err(error)
                if error
                    .downcast_ref::<FilesystemBoundaryError>()
                    .is_some_and(FilesystemBoundaryError::is_denied) =>
            {
                Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(localize_filesystem_boundary(
                        error.downcast_ref::<FilesystemBoundaryError>().unwrap(),
                    )),
                })
            }
            other => other,
        }
    }
}

// -- Helpers ------------------------------------------------------------------

#[derive(Debug)]
struct DataBoundaryViolation(String);

impl std::fmt::Display for DataBoundaryViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DataBoundaryViolation {}

fn data_boundary_violation(message: impl Into<String>) -> anyhow::Error {
    DataBoundaryViolation(message.into()).into()
}

fn tool_text_arg(key: &str, name: &str, value: &str) -> String {
    crate::i18n::get_required_tool_string_with_args(key, &[(name, value)])
}

fn localize_filesystem_boundary(error: &FilesystemBoundaryError) -> String {
    let (key, path) = error
        .localization()
        .expect("denied boundary has localization");
    crate::i18n::get_required_tool_string_with_args(key, &[("path", &path)])
}

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn count_files_older_than(dir: &Dir, cutoff_epoch: u64) -> anyhow::Result<usize> {
    let mut count = 0;
    for entry in dir.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let metadata = dir.symlink_metadata(&name)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            count +=
                count_files_older_than(&open_dir_nofollow(dir, Path::new(&name))?, cutoff_epoch)?;
        } else if file_type.is_file() {
            let modified = metadata
                .modified()
                .map(cap_std::time::SystemTime::into_std)?;
            let epoch = modified
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if epoch < cutoff_epoch {
                count += 1;
            }
        }
    }
    Ok(count)
}

struct PurgeCandidate {
    parent: Arc<Dir>,
    name: OsString,
    staging_name: OsString,
    resolved: PathBuf,
    bytes: u64,
    dev: u64,
    ino: u64,
}

impl PurgeCandidate {
    fn revalidate_name(&self, name: &std::ffi::OsStr, cutoff_epoch: u64) -> anyhow::Result<()> {
        let metadata = self.parent.symlink_metadata(name)?;
        let modified = metadata
            .modified()
            .map(cap_std::time::SystemTime::into_std)?;
        let epoch = modified
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if !metadata.is_file()
            || metadata.is_symlink()
            || metadata.dev() != self.dev
            || metadata.ino() != self.ino
            || epoch >= cutoff_epoch
        {
            return Err(data_boundary_violation(tool_text_arg(
                "tool-data-management-error-candidate-changed",
                "path",
                &self.resolved.display().to_string(),
            )));
        }
        Ok(())
    }
}

fn stage_purge_candidates(candidates: &[PurgeCandidate], cutoff_epoch: u64) -> anyhow::Result<()> {
    for candidate in candidates {
        match candidate.parent.symlink_metadata(&candidate.staging_name) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(data_boundary_violation(tool_text_arg(
                    "tool-data-management-error-staging-exists",
                    "path",
                    &candidate.staging_name.to_string_lossy(),
                )));
            }
            Err(error) => return Err(error.into()),
        }
    }

    let mut staged = 0usize;
    for candidate in candidates {
        if let Err(error) = rename_noreplace(
            &candidate.parent,
            Path::new(&candidate.name),
            Path::new(&candidate.staging_name),
        ) {
            return Err(with_rollback_error(error.into(), &candidates[..staged]));
        }
        staged += 1;
        if let Err(error) = candidate.revalidate_name(&candidate.staging_name, cutoff_epoch) {
            return Err(with_rollback_error(error, &candidates[..staged]));
        }
    }
    Ok(())
}

fn rollback_staged_candidates(candidates: &[PurgeCandidate]) -> anyhow::Result<()> {
    let mut failures = Vec::new();
    for candidate in candidates.iter().rev() {
        match candidate.parent.symlink_metadata(&candidate.name) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(error) = rename_noreplace(
                    &candidate.parent,
                    Path::new(&candidate.staging_name),
                    Path::new(&candidate.name),
                ) {
                    failures.push(format!("{}: {error}", candidate.resolved.display()));
                }
            }
            Ok(_) => failures.push(format!(
                "{}: original path was recreated during rollback",
                candidate.resolved.display()
            )),
            Err(error) => failures.push(format!(
                "{}: failed to inspect original path during rollback: {error}",
                candidate.resolved.display()
            )),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "Failed to restore quarantined retention files: {}",
            failures.join("; ")
        )
    }
}

fn with_rollback_error(original: anyhow::Error, candidates: &[PurgeCandidate]) -> anyhow::Error {
    match rollback_staged_candidates(candidates) {
        Ok(()) => original,
        Err(rollback) => anyhow::anyhow!("{original}; {rollback}"),
    }
}

fn delete_staged_candidates(candidates: &[PurgeCandidate]) -> anyhow::Result<()> {
    for (index, candidate) in candidates.iter().enumerate() {
        if let Err(error) = candidate.parent.remove_file(&candidate.staging_name) {
            return Err(with_rollback_error(error.into(), &candidates[index..]));
        }
    }
    Ok(())
}

fn collect_purge_candidates(
    dir: &Arc<Dir>,
    relative: &Path,
    workspace_path: &Path,
    cutoff_epoch: u64,
    candidates: &mut Vec<PurgeCandidate>,
) -> anyhow::Result<()> {
    for entry in dir.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let entry_relative = relative.join(&name);
        let metadata = dir.symlink_metadata(&name)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let child = Arc::new(open_dir_nofollow(dir, Path::new(&name))?);
            collect_purge_candidates(
                &child,
                &entry_relative,
                workspace_path,
                cutoff_epoch,
                candidates,
            )?;
        } else if file_type.is_file() {
            let modified = metadata
                .modified()
                .map(cap_std::time::SystemTime::into_std)?;
            let epoch = modified
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            if epoch < cutoff_epoch {
                static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);
                let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let staging_name = OsString::from(format!(
                    ".zeroclaw-purge-{}-{}-{}-{sequence}",
                    std::process::id(),
                    metadata.dev(),
                    metadata.ino()
                ));
                candidates.push(PurgeCandidate {
                    parent: dir.clone(),
                    name,
                    staging_name,
                    resolved: workspace_path.join(&entry_relative),
                    bytes: metadata.len(),
                    dev: metadata.dev(),
                    ino: metadata.ino(),
                });
            }
        }
    }
    Ok(())
}

fn dir_stats(root: &Dir) -> anyhow::Result<(usize, u64, serde_json::Value)> {
    let mut total_files = 0usize;
    let mut total_bytes = 0u64;
    let mut breakdown = serde_json::Map::new();

    for entry in root.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let metadata = root.symlink_metadata(&name)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let display_name = name.to_string_lossy().to_string();
            let (f, b) = count_dir_contents(&open_dir_nofollow(root, Path::new(&name))?)?;
            total_files += f;
            total_bytes += b;
            breakdown.insert(
                display_name,
                json!({"files": f, "size": b, "size_human": format_bytes(b)}),
            );
        } else if file_type.is_file() {
            total_files += 1;
            total_bytes += metadata.len();
        }
    }
    Ok((
        total_files,
        total_bytes,
        serde_json::Value::Object(breakdown),
    ))
}

fn count_dir_contents(dir: &Dir) -> anyhow::Result<(usize, u64)> {
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in dir.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let metadata = dir.symlink_metadata(&name)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let (f, b) = count_dir_contents(&open_dir_nofollow(dir, Path::new(&name))?)?;
            files += f;
            bytes += b;
        } else if file_type.is_file() {
            files += 1;
            bytes += metadata.len();
        }
    }
    Ok((files, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_constructor_remains_available() {
        let _ = DataManagementTool::new(std::env::temp_dir().join("workspace"), 30);
    }
    use tempfile::TempDir;
    use zeroclaw_config::autonomy::AutonomyLevel;

    fn make_tool(tmp: &TempDir) -> DataManagementTool {
        make_tool_with_autonomy(tmp, AutonomyLevel::Supervised)
    }

    fn make_tool_with_autonomy(tmp: &TempDir, autonomy: AutonomyLevel) -> DataManagementTool {
        let security = Arc::new(SecurityPolicy {
            autonomy,
            workspace_dir: tmp.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        DataManagementTool::new_with_security(90, security)
    }

    #[tokio::test]
    async fn retention_status_reports_correct_cutoff() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let res = tool
            .execute(json!({"command": "retention_status"}))
            .await
            .unwrap();
        assert!(res.success);
        let v: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        assert_eq!(v["retention_days"], 90);
        assert!(v["cutoff"].is_string());
    }

    #[tokio::test]
    async fn purge_dry_run_does_not_delete() {
        let tmp = TempDir::new().unwrap();
        // Create a file with an old modification time by writing it (it will have
        // the current mtime, so it should not be purged with a 90-day retention).
        std::fs::write(tmp.path().join("recent.txt"), "data").unwrap();

        let tool = make_tool(&tmp);
        let res = tool
            .execute(json!({"command": "purge", "dry_run": true}))
            .await
            .unwrap();
        assert!(res.success);
        let v: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        assert_eq!(v["dry_run"], true);
        // Recent file should not be counted for purge.
        assert_eq!(v["files"], 0);
        // File still exists.
        assert!(tmp.path().join("recent.txt").exists());
    }

    #[cfg(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "redox"
    ))]
    #[tokio::test]
    async fn confirmed_purge_deletes_eligible_regular_file() {
        let tmp = TempDir::new().unwrap();
        let old = tmp.path().join("old.txt");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&old)
            .unwrap();
        file.set_len(4).unwrap();
        file.set_modified(
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400),
        )
        .unwrap();
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: tmp.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        let tool = DataManagementTool::new_with_security(1, security);

        let result = tool
            .execute(json!({"command": "purge", "dry_run": false}))
            .await
            .unwrap();

        assert!(result.success, "error: {:?}", result.error);
        assert!(!old.exists());
    }

    #[tokio::test]
    async fn read_only_policy_blocks_destructive_purge() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("keep.txt"), "data").unwrap();
        let tool = make_tool_with_autonomy(&tmp, AutonomyLevel::ReadOnly);

        let result = tool
            .execute(json!({"command": "purge", "dry_run": false}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(tmp.path().join("keep.txt").exists());
    }

    #[cfg(all(
        unix,
        any(
            target_vendor = "apple",
            target_os = "linux",
            target_os = "android",
            target_os = "redox"
        )
    ))]
    #[tokio::test]
    async fn retention_walks_do_not_follow_symlinked_directories() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("outside.txt"), "outside").unwrap();
        symlink(outside.path(), tmp.path().join("linked-outside")).unwrap();
        let tool = make_tool(&tmp);

        let stats = tool.execute(json!({"command": "stats"})).await.unwrap();
        let stats: serde_json::Value = serde_json::from_str(&stats.output).unwrap();
        assert_eq!(stats["total_files"], 0);

        let purge = tool
            .execute(json!({"command": "purge", "dry_run": false}))
            .await
            .unwrap();
        assert!(purge.success);
        assert!(outside.path().join("outside.txt").exists());
    }

    #[test]
    fn purge_candidate_rejects_replaced_file_identity() {
        let tmp = TempDir::new().unwrap();
        let original = tmp.path().join("old.txt");
        std::fs::write(&original, "old").unwrap();
        let workspace = Arc::new(
            open_absolute_dir_nofollow(&std::fs::canonicalize(tmp.path()).unwrap()).unwrap(),
        );
        let mut candidates = Vec::new();
        collect_purge_candidates(
            &workspace,
            Path::new(""),
            tmp.path(),
            u64::MAX,
            &mut candidates,
        )
        .unwrap();
        assert_eq!(candidates.len(), 1);

        std::fs::rename(&original, tmp.path().join("moved.txt")).unwrap();
        std::fs::write(&original, "replacement").unwrap();

        assert!(stage_purge_candidates(&candidates, u64::MAX).is_err());
        assert_eq!(std::fs::read_to_string(original).unwrap(), "replacement");
    }

    #[test]
    fn purge_staging_collision_causes_zero_mutation() {
        let tmp = TempDir::new().unwrap();
        let original = tmp.path().join("old.txt");
        std::fs::write(&original, "old").unwrap();
        let parent =
            Arc::new(Dir::open_ambient_dir(tmp.path(), cap_std::ambient_authority()).unwrap());
        let metadata = parent.symlink_metadata("old.txt").unwrap();
        let staging_name = OsString::from("occupied-stage");
        std::fs::write(tmp.path().join(&staging_name), "occupied").unwrap();
        let candidate = PurgeCandidate {
            parent,
            name: OsString::from("old.txt"),
            staging_name,
            resolved: original.clone(),
            bytes: metadata.len(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        };

        assert!(stage_purge_candidates(&[candidate], u64::MAX).is_err());
        assert_eq!(std::fs::read_to_string(original).unwrap(), "old");
    }

    #[test]
    fn rollback_reports_recreated_original_path() {
        let tmp = TempDir::new().unwrap();
        let original = tmp.path().join("old.txt");
        let staged = tmp.path().join("stage");
        std::fs::write(&staged, "quarantined").unwrap();
        std::fs::write(&original, "replacement").unwrap();
        let parent =
            Arc::new(Dir::open_ambient_dir(tmp.path(), cap_std::ambient_authority()).unwrap());
        let metadata = parent.symlink_metadata("stage").unwrap();
        let candidate = PurgeCandidate {
            parent,
            name: OsString::from("old.txt"),
            staging_name: OsString::from("stage"),
            resolved: original,
            bytes: metadata.len(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        };

        let error = rollback_staged_candidates(&[candidate]).unwrap_err();
        assert!(error.to_string().contains("recreated"));
        assert_eq!(std::fs::read_to_string(staged).unwrap(), "quarantined");
    }

    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "redox"
    )))]
    #[tokio::test]
    async fn confirmed_purge_fails_closed_when_quarantine_is_unsupported() {
        let tmp = TempDir::new().unwrap();
        let missing_workspace = tmp.path().join("missing");
        let tool = DataManagementTool::new(missing_workspace, 90);

        let result = tool
            .execute(json!({"command": "purge", "dry_run": false}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("unavailable")
        );
    }

    #[tokio::test]
    async fn stats_counts_files_correctly() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), "hello").unwrap();
        std::fs::write(sub.join("b.txt"), "world").unwrap();
        std::fs::write(tmp.path().join("root.txt"), "top").unwrap();

        let tool = make_tool(&tmp);
        let res = tool.execute(json!({"command": "stats"})).await.unwrap();
        assert!(res.success);
        let v: serde_json::Value = serde_json::from_str(&res.output).unwrap();
        assert_eq!(v["total_files"], 3);
    }
}
