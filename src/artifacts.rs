use std::{
    collections::HashMap,
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::leases::{AgentSessionId, BrowserToolError, TabId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactSummary {
    pub artifact_id: String,
    pub kind: String,
    pub media_type: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub created_at_ms: u64,
    pub retention: String,
}

#[derive(Debug, Clone)]
pub struct ArtifactRecord {
    pub summary: ArtifactSummary,
    pub owner_session_id: AgentSessionId,
    pub tab_id: Option<TabId>,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct ArtifactRegistry {
    root: PathBuf,
    records: HashMap<String, ArtifactRecord>,
}

impl ArtifactRegistry {
    pub fn new(state_root: &Path) -> Result<Self, BrowserToolError> {
        let artifacts_root = state_root.join("artifacts");
        fs::create_dir_all(&artifacts_root).map_err(|error| {
            BrowserToolError::artifact_error(format!(
                "failed to create artifact root `{}`: {error}",
                artifacts_root.display()
            ))
        })?;
        for entry in fs::read_dir(&artifacts_root).map_err(|error| {
            BrowserToolError::artifact_error(format!(
                "failed to inspect artifact root `{}`: {error}",
                artifacts_root.display()
            ))
        })? {
            let entry = entry.map_err(|error| {
                BrowserToolError::artifact_error(format!(
                    "failed to inspect artifact generation: {error}"
                ))
            })?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("generation_"))
                && entry
                    .file_type()
                    .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?
                    .is_dir()
            {
                fs::remove_dir_all(entry.path()).map_err(|error| {
                    BrowserToolError::artifact_error(format!(
                        "failed to remove expired artifact generation `{}`: {error}",
                        entry.path().display()
                    ))
                })?;
            }
        }
        let root = artifacts_root.join(format!("generation_{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&root).map_err(|error| {
            BrowserToolError::artifact_error(format!(
                "failed to create artifact directory `{}`: {error}",
                root.display()
            ))
        })?;
        Ok(Self {
            root,
            records: HashMap::new(),
        })
    }

    pub fn insert_bytes(
        &mut self,
        session_id: &AgentSessionId,
        tab_id: Option<&TabId>,
        kind: &str,
        media_type: &str,
        extension: &str,
        bytes: &[u8],
    ) -> Result<ArtifactSummary, BrowserToolError> {
        let artifact_id = format!("artifact_{}", Uuid::new_v4());
        let session_dir = self.root.join(&session_id.0);
        fs::create_dir_all(&session_dir).map_err(|error| {
            BrowserToolError::artifact_error(format!(
                "failed to create session artifact directory: {error}"
            ))
        })?;
        let path = session_dir.join(format!("{artifact_id}.{extension}"));
        atomic_write(&path, bytes)?;
        let summary = ArtifactSummary {
            artifact_id: artifact_id.clone(),
            kind: kind.to_string(),
            media_type: media_type.to_string(),
            size_bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
            created_at_ms: now_ms(),
            retention: "session".to_string(),
        };
        self.records.insert(
            artifact_id,
            ArtifactRecord {
                summary: summary.clone(),
                owner_session_id: session_id.clone(),
                tab_id: tab_id.cloned(),
                path,
            },
        );
        Ok(summary)
    }

    pub fn list(
        &self,
        session_id: &AgentSessionId,
        tab_id: Option<&TabId>,
        kinds: &[String],
    ) -> Vec<ArtifactSummary> {
        let mut artifacts = self
            .records
            .values()
            .filter(|record| &record.owner_session_id == session_id)
            .filter(|record| tab_id.is_none_or(|tab_id| record.tab_id.as_ref() == Some(tab_id)))
            .filter(|record| kinds.is_empty() || kinds.contains(&record.summary.kind))
            .map(|record| record.summary.clone())
            .collect::<Vec<_>>();
        artifacts.sort_by_key(|artifact| artifact.created_at_ms);
        artifacts
    }

    pub fn metadata(
        &self,
        session_id: &AgentSessionId,
        artifact_id: &str,
    ) -> Result<ArtifactSummary, BrowserToolError> {
        Ok(self.record(session_id, artifact_id)?.summary.clone())
    }

    pub fn read(
        &self,
        session_id: &AgentSessionId,
        artifact_id: &str,
        offset: u64,
        length: usize,
    ) -> Result<(Vec<u8>, bool), BrowserToolError> {
        let record = self.record(session_id, artifact_id)?;
        let bounded_length = length.clamp(1, 1_048_576);
        let mut file = fs::File::open(&record.path)
            .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
        let mut bytes = vec![0; bounded_length];
        let read = file
            .read(&mut bytes)
            .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
        bytes.truncate(read);
        Ok((
            bytes,
            offset.saturating_add(read as u64) < record.summary.size_bytes,
        ))
    }

    pub fn bytes(
        &self,
        session_id: &AgentSessionId,
        artifact_id: &str,
    ) -> Result<Vec<u8>, BrowserToolError> {
        let record = self.record(session_id, artifact_id)?;
        fs::read(&record.path).map_err(|error| BrowserToolError::artifact_error(error.to_string()))
    }

    pub fn export(
        &self,
        session_id: &AgentSessionId,
        artifact_id: &str,
        workspace_root: &Path,
        requested_path: &Path,
        overwrite: bool,
    ) -> Result<PathBuf, BrowserToolError> {
        let record = self.record(session_id, artifact_id)?;
        let workspace_root = workspace_root.canonicalize().map_err(|error| {
            BrowserToolError::workspace_unavailable(format!(
                "workspace root `{}` is unavailable: {error}",
                workspace_root.display()
            ))
        })?;
        let destination = safe_workspace_destination(&workspace_root, requested_path)?;
        if destination.exists() && !overwrite {
            return Err(BrowserToolError::artifact_error(format!(
                "export destination `{}` already exists",
                destination.display()
            )));
        }
        let bytes = fs::read(&record.path)
            .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
        atomic_write(&destination, &bytes)?;
        Ok(destination)
    }

    pub fn delete(
        &mut self,
        session_id: &AgentSessionId,
        artifact_id: &str,
    ) -> Result<(), BrowserToolError> {
        let record = self.record(session_id, artifact_id)?.clone();
        fs::remove_file(&record.path)
            .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
        self.records.remove(artifact_id);
        Ok(())
    }

    pub fn remove_session(&mut self, session_id: &AgentSessionId) {
        let artifact_ids = self
            .records
            .iter()
            .filter(|(_, record)| &record.owner_session_id == session_id)
            .map(|(artifact_id, _)| artifact_id.clone())
            .collect::<Vec<_>>();
        for artifact_id in artifact_ids {
            if let Some(record) = self.records.remove(&artifact_id) {
                let _ = fs::remove_file(record.path);
            }
        }
    }

    fn record(
        &self,
        session_id: &AgentSessionId,
        artifact_id: &str,
    ) -> Result<&ArtifactRecord, BrowserToolError> {
        self.records
            .get(artifact_id)
            .filter(|record| &record.owner_session_id == session_id)
            .ok_or_else(|| BrowserToolError::artifact_not_found(artifact_id))
    }
}

fn safe_workspace_destination(
    workspace_root: &Path,
    requested_path: &Path,
) -> Result<PathBuf, BrowserToolError> {
    if requested_path.is_absolute() {
        return Err(BrowserToolError::path_outside_workspace(requested_path));
    }
    if requested_path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(BrowserToolError::path_outside_workspace(requested_path));
    }
    let destination = workspace_root.join(requested_path);
    let parent = destination
        .parent()
        .ok_or_else(|| BrowserToolError::path_outside_workspace(requested_path))?;
    let canonical_parent = parent.canonicalize().map_err(|error| {
        BrowserToolError::workspace_unavailable(format!(
            "export parent `{}` is unavailable: {error}",
            parent.display()
        ))
    })?;
    if !canonical_parent.starts_with(workspace_root) {
        return Err(BrowserToolError::path_outside_workspace(requested_path));
    }
    if destination
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(BrowserToolError::path_outside_workspace(requested_path));
    }
    Ok(canonical_parent.join(
        destination
            .file_name()
            .ok_or_else(|| BrowserToolError::path_outside_workspace(requested_path))?,
    ))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), BrowserToolError> {
    let parent = path.parent().ok_or_else(|| {
        BrowserToolError::artifact_error(format!("path `{}` has no parent", path.display()))
    })?;
    let temporary = parent.join(format!(".vbl-{}.tmp", Uuid::new_v4().simple()));
    let mut file = fs::File::create(&temporary)
        .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
    fs::rename(&temporary, path)
        .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn artifacts_are_session_owned_and_export_inside_workspace() {
        let state = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let session = AgentSessionId("session_one".to_string());
        let foreign = AgentSessionId("session_two".to_string());
        let mut registry = ArtifactRegistry::new(state.path()).unwrap();
        let artifact = registry
            .insert_bytes(&session, None, "audit", "application/json", "json", b"{}")
            .unwrap();

        assert!(registry.metadata(&foreign, &artifact.artifact_id).is_err());
        let path = registry
            .export(
                &session,
                &artifact.artifact_id,
                workspace.path(),
                Path::new("report.json"),
                false,
            )
            .unwrap();
        assert_eq!(fs::read(path).unwrap(), b"{}");
        assert!(
            registry
                .export(
                    &session,
                    &artifact.artifact_id,
                    workspace.path(),
                    Path::new("../outside.json"),
                    false,
                )
                .is_err()
        );
        assert!(
            registry
                .export(
                    &session,
                    &artifact.artifact_id,
                    workspace.path(),
                    &workspace.path().join("absolute.json"),
                    false,
                )
                .is_err()
        );
    }

    #[test]
    fn new_registry_removes_unreachable_generations() {
        let state = tempdir().unwrap();
        let expired = state.path().join("artifacts/generation_expired");
        fs::create_dir_all(&expired).unwrap();
        fs::write(expired.join("artifact.bin"), b"expired").unwrap();
        fs::write(state.path().join("artifacts/keep.txt"), b"keep").unwrap();

        let registry = ArtifactRegistry::new(state.path()).unwrap();

        assert!(!expired.exists());
        assert!(state.path().join("artifacts/keep.txt").is_file());
        assert!(registry.root.is_dir());
    }
}
