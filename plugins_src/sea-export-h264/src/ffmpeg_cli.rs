//! ユーザー導入済み外部 `ffmpeg` CLI を別プロセス起動して H.264 / AAC MP4 を生成するバックエンド。
//!
//! バイナリにはいかなる GPL / 特許コーデックも同梱せず、指定された外部実行ファイルのみを呼び出す。

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

use sea_export_protocol::ExportInitPayload;

/// 外部 `ffmpeg` 実行プロセスと一時音声バッファを管理するセッション。
pub struct ExternalFfmpegSession {
    ffmpeg_cmd: String,
    sample_rate: u32,
    audio_channels: u16,
    output_path: PathBuf,
    temp_video_path: PathBuf,
    temp_audio_path: PathBuf,
    video_child: Option<Child>,
    video_stdin: Option<BufWriter<ChildStdin>>,
    audio_writer: Option<BufWriter<File>>,
    audio_bytes_written: u64,
    frames_submitted: u64,
    audio_samples_received: u64,
}

impl ExternalFfmpegSession {
    /// 外部 `ffmpeg` を起動し、`stdin` からの生 BGRA ストリーム受付状態に入る。
    ///
    /// # Errors
    ///
    /// 指定コマンドが存在しない・起動できない・作業ファイル作成失敗時に明確なエラー文字列を返す。
    pub fn start(
        init: &ExportInitPayload,
        ffmpeg_cmd: &str,
        bitrate_kbps: u32,
    ) -> Result<Self, String> {
        let trimmed = ffmpeg_cmd.trim();
        if trimmed.is_empty() {
            return Err("外部 ffmpeg コマンドパスが空です".to_owned());
        }

        let output_path = PathBuf::from(&init.output_path);
        if let Some(parent) = output_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("出力先フォルダを作成できません: {err}"))?;
        }

        let pid = std::process::id();
        let temp_dir = std::env::temp_dir();
        let temp_video_path = temp_dir.join(format!("sea-ext-ffmpeg-{pid}-video.mp4"));
        let temp_audio_path = temp_dir.join(format!("sea-ext-ffmpeg-{pid}-audio.f32"));

        let audio_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp_audio_path)
            .map_err(|err| format!("一時音声ファイル作成に失敗しました: {err}"))?;

        let size_arg = format!("{}x{}", init.width, init.height);
        let rate_arg = format!("{}/{}", init.fps_num.max(1), init.fps_den.max(1));
        let bitrate_arg = format!("{bitrate_kbps}k");
        let temp_video_str = temp_video_path.to_string_lossy().into_owned();

        let mut cmd = Command::new(trimmed);
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "rawvideo",
            "-pixel_format",
            "bgra",
            "-video_size",
            &size_arg,
            "-framerate",
            &rate_arg,
            "-i",
            "pipe:0",
            "-an",
            "-c:v",
            "libx264",
            "-b:v",
            &bitrate_arg,
            "-pix_fmt",
            "yuv420p",
            &temp_video_str,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|err| {
            let _ = std::fs::remove_file(&temp_audio_path);
            format!(
                "外部 ffmpeg ('{trimmed}') を起動できませんでした ({err})。OS Media Foundation モードを使うか、ffmpeg を導入・パス指定してください。"
            )
        })?;

        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&temp_audio_path);
            return Err("外部 ffmpeg の stdin パイプを取得できませんでした".to_owned());
        };

        Ok(Self {
            ffmpeg_cmd: trimmed.to_owned(),
            sample_rate: init.sample_rate,
            audio_channels: init.audio_channels,
            output_path,
            temp_video_path,
            temp_audio_path,
            video_child: Some(child),
            video_stdin: Some(BufWriter::new(stdin)),
            audio_writer: Some(BufWriter::new(audio_file)),
            audio_bytes_written: 0,
            frames_submitted: 0,
            audio_samples_received: 0,
        })
    }

    /// 1 フレーム分の `BGRA8` バイト列を外部 `ffmpeg` の `stdin` へ送出する。
    ///
    /// # Errors
    ///
    /// パイプ書き込み失敗時にエラー文字列を返す。
    pub fn write_video_bgra8(&mut self, bgra: &[u8]) -> Result<(), String> {
        let stdin = self
            .video_stdin
            .as_mut()
            .ok_or_else(|| "外部 ffmpeg 映像パイプは既に閉じられています".to_owned())?;
        stdin
            .write_all(bgra)
            .map_err(|err| format!("外部 ffmpeg 映像ストリーム書き込み失敗: {err}"))?;
        self.frames_submitted = self.frames_submitted.saturating_add(1);
        Ok(())
    }

    /// インターリーブ `f32` LE バイト列を一時音声ファイルへ追記する。
    ///
    /// # Errors
    ///
    /// ファイル書き込み失敗時にエラー文字列を返す。
    pub fn write_audio_f32(
        &mut self,
        sample_frame_count: u32,
        raw_f32_le_bytes: &[u8],
    ) -> Result<(), String> {
        if let Some(writer) = self.audio_writer.as_mut() {
            writer
                .write_all(raw_f32_le_bytes)
                .map_err(|err| format!("外部 ffmpeg 用一時音声書き込み失敗: {err}"))?;
            self.audio_bytes_written = self
                .audio_bytes_written
                .saturating_add(u64::try_from(raw_f32_le_bytes.len()).unwrap_or(0));
            self.audio_samples_received = self
                .audio_samples_received
                .saturating_add(u64::from(sample_frame_count));
        }
        Ok(())
    }

    /// 現時点で投入済みの映像フレーム数を返す。
    #[must_use]
    pub const fn frames_submitted(&self) -> u64 {
        self.frames_submitted
    }

    /// 現時点で投入済みの音声サンプル数を返す。
    #[must_use]
    pub const fn audio_samples_received(&self) -> u64 {
        self.audio_samples_received
    }

    /// 映像エンコードを確定し、音声がある場合は MP4 へ AAC 多重化して出力ファイルサイズを返す。
    ///
    /// # Errors
    ///
    /// 外部 `ffmpeg` の終了コード異常または出力ファイル生成失敗時にエラー文字列を返す。
    pub fn finalize(mut self) -> Result<(u64, u64), String> {
        if let Some(mut stdin) = self.video_stdin.take() {
            stdin
                .flush()
                .map_err(|err| format!("外部 ffmpeg stdin フラッシュ失敗: {err}"))?;
        }
        if let Some(mut aw) = self.audio_writer.take() {
            aw.flush()
                .map_err(|err| format!("一時音声ファイルフラッシュ失敗: {err}"))?;
        }

        let child = self
            .video_child
            .take()
            .ok_or_else(|| "外部 ffmpeg プロセスが保持されていません".to_owned())?;
        let output = child
            .wait_with_output()
            .map_err(|err| format!("外部 ffmpeg 終了待機に失敗しました: {err}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "外部 ffmpeg 映像エンコード失敗 ({}): {}",
                output.status,
                stderr.trim()
            ));
        }

        if self.audio_bytes_written == 0 || self.audio_channels == 0 {
            std::fs::copy(&self.temp_video_path, &self.output_path)
                .map_err(|err| format!("出力 MP4 コピー失敗: {err}"))?;
        } else {
            self.mux_video_and_audio()?;
        }

        let size = std::fs::metadata(&self.output_path)
            .map(|m| m.len())
            .map_err(|err| format!("出力 MP4 サイズ取得失敗: {err}"))?;
        Ok((size, self.frames_submitted))
    }

    fn mux_video_and_audio(&self) -> Result<(), String> {
        let temp_vid = self.temp_video_path.to_string_lossy().into_owned();
        let temp_aud = self.temp_audio_path.to_string_lossy().into_owned();
        let out_str = self.output_path.to_string_lossy().into_owned();
        let sr_arg = self.sample_rate.to_string();
        let ch_arg = self.audio_channels.to_string();

        let output = Command::new(&self.ffmpeg_cmd)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                &temp_vid,
                "-f",
                "f32le",
                "-ar",
                &sr_arg,
                "-ac",
                &ch_arg,
                "-i",
                &temp_aud,
                "-c:v",
                "copy",
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-movflags",
                "+faststart",
                &out_str,
            ])
            .output()
            .map_err(|err| format!("外部 ffmpeg 音声多重化起動失敗: {err}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "外部 ffmpeg 音声多重化失敗 ({}): {}",
                output.status,
                stderr.trim()
            ));
        }
        Ok(())
    }
}

impl Drop for ExternalFfmpegSession {
    fn drop(&mut self) {
        self.video_stdin.take();
        self.audio_writer.take();
        if let Some(mut child) = self.video_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.temp_video_path);
        let _ = std::fs::remove_file(&self.temp_audio_path);
    }
}

#[cfg(test)]
mod tests {
    use sea_export_protocol::{
        AudioSampleFormat, EXPORT_PROTOCOL_VERSION, PixelFormat, ShmLayout, ShmLayoutSpec,
    };

    use super::*;

    #[test]
    fn missing_external_ffmpeg_binary_returns_clear_error() {
        let layout = ShmLayout::from_spec(ShmLayoutSpec {
            width: 32,
            height: 32,
            pixel_bytes: 4,
            video_slot_count: 2,
            audio_channels: 2,
            audio_slot_capacity_frames: 1024,
            audio_slot_count: 2,
        })
        .unwrap();
        let init = ExportInitPayload {
            protocol_version: EXPORT_PROTOCOL_VERSION,
            width: 32,
            height: 32,
            fps_num: 30,
            fps_den: 1,
            total_video_frames: 1,
            sample_rate: 48_000,
            audio_channels: 2,
            pixel_format: PixelFormat::Bgra8,
            audio_sample_format: AudioSampleFormat::F32Interleaved,
            shm_path: "unused.shm".to_owned(),
            shm_layout: layout,
            output_path: std::env::temp_dir()
                .join("sea-missing-ffmpeg-out.mp4")
                .to_string_lossy()
                .into_owned(),
            preset_id: "h264_external_ffmpeg".to_owned(),
            options: serde_json::json!({}),
        };

        let err = ExternalFfmpegSession::start(&init, "nonexistent-ffmpeg-binary-xyz123", 5000)
            .err()
            .unwrap();
        assert!(err.contains("nonexistent-ffmpeg-binary-xyz123"));
        assert!(!PathBuf::from(&init.output_path).exists());
    }
}
