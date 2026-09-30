//! Mux already downloaded tracks. No network requests or re-encoding.
use crate::{Error, Result};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

pub async fn mux(
    video: &Path,
    audio: &Path,
    output: &Path,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<()> {
    let binary = std::env::var_os("HAZAR_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ffmpeg"));
    let tmp = output.with_extension("muxing");
    let format = if output.extension().is_some_and(|e| e == "ts") {
        "mpegts"
    } else if output.extension().is_some_and(|e| e == "mkv") {
        "matroska"
    } else {
        "mp4"
    };
    let mut command = tokio::process::Command::new(binary);
    command
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-protocol_whitelist", "file,pipe", "-i"])
        .arg(video)
        .args(["-protocol_whitelist", "file,pipe", "-i"])
        .arg(audio)
        .args(["-map", "0:v:0", "-map", "1:a:0", "-c", "copy", "-f", format])
        .arg(&tmp)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command
        .spawn()
        .map_err(|_| Error::Unsupported("FFmpeg bulunamadı; uygulamayı yeniden kur".into()))?;
    loop {
        if cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
            child.kill().await?;
            return Err(Error::Cancelled);
        }
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                return Err(Error::Protocol(
                    "Audio/video birleştirilemedi; track dosyaları korundu".into(),
                ));
            }
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if tokio::fs::metadata(&tmp).await?.len() == 0 {
        return Err(Error::Protocol("Boş medya output".into()));
    }
    tokio::fs::rename(&tmp, output).await?;
    Ok(())
}
