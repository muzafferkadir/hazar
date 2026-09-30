//! Real media output, including both tracks. CI uses the bundled executable.
use std::{path::PathBuf, process::Command};
#[tokio::test]
async fn mux_keeps_audio_and_video_playable() {
    let bundled = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../src-tauri/media").join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
    let binary = if bundled.exists() { bundled } else { PathBuf::from("ffmpeg") };
    let dir = std::env::temp_dir().join(format!("hazar-mux-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let video = dir.join("video.mp4"); let audio = dir.join("audio.m4a"); let output = dir.join("output.mp4");
    for (filter, codec, path) in [("color=c=black:s=64x64:r=10", "mpeg4", &video), ("sine=frequency=440:sample_rate=44100", "aac", &audio)] {
        let codec_flag = if codec == "aac" { "-c:a" } else { "-c:v" };
        let result = Command::new(&binary).args(["-nostdin", "-loglevel", "error", "-y", "-f", "lavfi", "-i", filter, "-t", "1", codec_flag, codec]).arg(path).status().expect("FFmpeg required for media regression test");
        assert!(result.success());
    }
    std::env::set_var("HAZAR_FFMPEG", &binary);
    hazar_engine::media::mux(&video, &audio, &output, None).await.unwrap();
    for track in ["0:v:0", "0:a:0"] {
        assert!(Command::new(&binary).args(["-nostdin", "-loglevel", "error", "-i"]).arg(&output).args(["-map", track, "-f", "null", "-"]).status().unwrap().success());
    }
    assert!(std::fs::metadata(output).unwrap().len() > 1000);
    std::fs::remove_dir_all(dir).unwrap();
}
