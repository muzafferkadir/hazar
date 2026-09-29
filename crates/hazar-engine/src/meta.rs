use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::error::{Error, Result};
use crate::probe::ResourceInfo;

pub const META_VERSION: u32 = 1;
pub const WORK_SUFFIX: &str = ".hazar";

/// One byte range of the file. `end` is inclusive, like HTTP ranges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartState {
    pub index: u32,
    pub start: u64,
    pub end: u64,
    /// Bytes of this part already on disk.
    pub written: u64,
}

impl PartState {
    pub fn length(&self) -> u64 {
        self.end.saturating_sub(self.start) + 1
    }

    pub fn done(&self) -> bool {
        self.written >= self.length()
    }
}

/// Resume state stored next to the target file in `<dest>.hazar/meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadMeta {
    pub version: u32,
    pub url: String,
    pub final_url: String,
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub connections: usize,
    pub parts: Vec<PartState>,
    pub created_at: u64,
}

impl DownloadMeta {
    pub fn new(
        url: &str,
        info: &ResourceInfo,
        size: u64,
        connections: usize,
        parts: Vec<PartState>,
    ) -> Self {
        Self {
            version: META_VERSION,
            url: url.to_string(),
            final_url: info.final_url.clone(),
            size,
            etag: info.etag.clone(),
            last_modified: info.last_modified.clone(),
            connections,
            parts,
            created_at: now_secs(),
        }
    }

    /// Can an existing meta file be resumed against what the server reports now?
    pub fn matches(&self, info: &ResourceInfo, size: u64) -> bool {
        if self.version != META_VERSION {
            return false;
        }
        if self.final_url != info.final_url || self.size != size {
            return false;
        }
        // Strong validators: if the server gave one before, it must match now.
        match (&self.etag, &info.etag) {
            (Some(old), Some(new)) if old != new => return false,
            (Some(_), None) => return false,
            _ => {}
        }
        if let (Some(old), Some(new)) = (&self.last_modified, &info.last_modified) {
            if old != new {
                return false;
            }
        }
        true
    }

    pub fn bytes_done(&self) -> u64 {
        self.parts.iter().map(|p| p.written).sum()
    }

    pub fn is_complete(&self) -> bool {
        self.parts.iter().all(|p| p.done())
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Layout of the sidecar directory: meta + one file per part.
#[derive(Debug, Clone)]
pub struct WorkDir {
    pub root: PathBuf,
}

impl WorkDir {
    pub fn new(dest: &Path) -> Self {
        let root = PathBuf::from(format!("{}{}", dest.display(), WORK_SUFFIX));
        Self { root }
    }

    pub fn meta_path(&self) -> PathBuf {
        self.root.join("meta.json")
    }

    pub fn part_path(&self, index: u32) -> PathBuf {
        self.root.join(format!("part-{index:03}.bin"))
    }

    pub async fn ensure(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.root).await?;
        Ok(())
    }

    pub async fn load_meta(&self) -> Result<Option<DownloadMeta>> {
        let path = self.meta_path();
        let raw = match tokio::fs::read(&path).await {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let meta: DownloadMeta = serde_json::from_slice(&raw).map_err(|e| Error::InvalidMeta {
            path,
            reason: e.to_string(),
        })?;
        Ok(Some(meta))
    }

    /// Atomic-ish write so a crash never leaves half a JSON file behind.
    pub async fn save_meta(&self, meta: &DownloadMeta) -> Result<()> {
        self.ensure().await?;
        let tmp = self.root.join("meta.json.tmp");
        let mut file = tokio::fs::File::create(&tmp).await?;
        file.write_all(&serde_json::to_vec_pretty(meta)?).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, self.meta_path()).await?;
        Ok(())
    }

    pub async fn part_len(&self, index: u32) -> Result<u64> {
        match tokio::fs::metadata(self.part_path(index)).await {
            Ok(m) => Ok(m.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn truncate_part(&self, index: u32) -> Result<()> {
        let path = self.part_path(index);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            tokio::fs::remove_file(path).await?;
        }
        Ok(())
    }

    /// Parts are written strictly sequentially, so append is the resume write.
    pub async fn open_part_append(&self, index: u32) -> Result<tokio::fs::File> {
        self.ensure().await?;
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.part_path(index))
            .await?;
        Ok(file)
    }

    pub async fn open_part_read(&self, index: u32) -> Result<tokio::fs::File> {
        Ok(tokio::fs::File::open(self.part_path(index)).await?)
    }

    pub async fn reset(&self) -> Result<()> {
        if tokio::fs::try_exists(&self.root).await.unwrap_or(false) {
            tokio::fs::remove_dir_all(&self.root).await?;
        }
        Ok(())
    }
}
