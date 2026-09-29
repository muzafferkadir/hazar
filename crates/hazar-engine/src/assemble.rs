use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{Error, Result};
use crate::meta::{DownloadMeta, WorkDir};

const BUF: usize = 1 << 20;

/// Concatenate the part files into `dest` in index order.
pub async fn assemble(work: &WorkDir, dest: &Path, meta: &DownloadMeta) -> Result<()> {
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    let tmp = dest.with_extension(match dest.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.assembling"),
        None => "assembling".to_string(),
    });

    {
        let mut out = tokio::fs::File::create(&tmp).await?;
        let mut buf = vec![0u8; BUF];
        for part in &meta.parts {
            let mut input = tokio::fs::File::open(work.part_path(part.index)).await?;
            let mut written = 0u64;
            loop {
                let n = input.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n]).await?;
                written += n as u64;
            }
            if written != part.length() {
                return Err(Error::ShortBody {
                    expected: part.length(),
                    got: written,
                });
            }
        }
        out.flush().await?;
        out.sync_all().await?;
    }

    let written = tokio::fs::metadata(&tmp).await?.len();
    if written != meta.size {
        return Err(Error::ShortBody {
            expected: meta.size,
            got: written,
        });
    }

    tokio::fs::rename(&tmp, dest).await?;
    Ok(())
}
