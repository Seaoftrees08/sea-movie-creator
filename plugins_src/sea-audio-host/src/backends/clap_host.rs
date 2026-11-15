//! 外部 `.clap` プラグインの `clack-host` ベース隔離ホストバックエンド。
#![allow(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::path::Path;

use clack_extensions::params::{
    HostParams, HostParamsImplMainThread, HostParamsImplShared, ParamClearFlags, ParamInfoBuffer,
    ParamInfoFlags, ParamRescanFlags, PluginParams,
};
use clack_host::events::Pckn;
use clack_host::events::event_types::ParamValueEvent;
use clack_host::events::io::{EventBuffer, InputEvents, OutputEvents};
use clack_host::factory::plugin::PluginFactory;
use clack_host::prelude::*;
use sea_export_protocol::audio_ipc::{
    AudioPluginDescriptor, AudioPluginParameterDescriptor, WireAudioPluginFormat,
};

struct SeaClapShared;

impl SharedHandler<'_> for SeaClapShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

impl HostParamsImplShared for SeaClapShared {
    fn request_flush(&self) {}
}

struct SeaClapMainThread;

impl MainThreadHandler<'_> for SeaClapMainThread {
    fn initialized(&self, _instance: InitializedPluginHandle<'_>) {}
}

impl HostParamsImplMainThread for SeaClapMainThread {
    fn rescan(&self, _flags: ParamRescanFlags) {}
    fn clear(&self, _param_id: ClapId, _flags: ParamClearFlags) {}
}

struct SeaClapHost;

impl HostHandlers for SeaClapHost {
    type Shared<'a> = SeaClapShared;
    type MainThread<'a> = SeaClapMainThread;
    type AudioProcessor<'a> = ();

    fn declare_extensions(builder: &mut HostExtensions<Self>, _: &Self::Shared<'_>) {
        builder.register::<HostParams>();
    }
}

/// ロード済み `CLAP` プラグインインスタンス（シングルワーカースレッド所有）。
pub struct ClapHostedPlugin {
    processor: Option<StartedPluginAudioProcessor<SeaClapHost>>,
    instance: PluginInstance<SeaClapHost>,
    descriptor: AudioPluginDescriptor,
    param_map: BTreeMap<String, ClapId>,
    pending_events: EventBuffer,
    output_events: EventBuffer,
    in_ports: AudioPorts,
    out_ports: AudioPorts,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    sample_cursor: u64,
}

impl Drop for ClapHostedPlugin {
    fn drop(&mut self) {
        if let Some(started) = self.processor.take() {
            let stopped = started.stop_processing();
            self.instance.deactivate(stopped);
        }
    }
}

/// 指定 `.clap` バンドルを読み取り、公開プラグイン一覧を返す。
///
/// # Errors
///
/// `.clap` エントリーロードまたはファクトリ取得に失敗した場合エラー詳細を返す。
pub fn describe_clap_bundle(bundle_path: &Path) -> Result<Vec<AudioPluginDescriptor>, String> {
    // SAFETY:
    // `.clap` 動的ライブラリを隔離された子プロセス `sea-audio-host` 内でロードし、
    // ファクトリ記述子のみを検査する。
    let entry = unsafe { PluginEntry::load(bundle_path) }
        .map_err(|e| format!("failed to load CLAP entry {}: {e}", bundle_path.display()))?;
    let factory = entry
        .get_factory::<PluginFactory>()
        .ok_or_else(|| "CLAP entry exposes no PluginFactory".to_owned())?;

    let mut out = Vec::new();
    for desc in factory.plugin_descriptors() {
        let plugin_id = desc
            .id()
            .map_or_else(String::new, |c| c.to_string_lossy().into_owned());
        if plugin_id.is_empty() {
            continue;
        }
        let name = desc
            .name()
            .map_or_else(|| plugin_id.clone(), |c| c.to_string_lossy().into_owned());
        let vendor = desc.vendor().map_or_else(
            || "Unknown".to_owned(),
            |c| c.to_string_lossy().into_owned(),
        );
        out.push(AudioPluginDescriptor {
            plugin_id,
            name,
            vendor,
            format: WireAudioPluginFormat::Clap,
            bundle_path: Some(bundle_path.to_string_lossy().into_owned()),
            parameters: Vec::new(),
        });
    }
    Ok(out)
}

impl ClapHostedPlugin {
    /// `.clap` バンドルから対象 `plugin_id` を生成し、指定レート・ブロック長で有効化する。
    ///
    /// # Errors
    ///
    /// ロード・生成・アクティベーション・処理開始のいずれかに失敗した場合エラー文を返す。
    pub fn load(
        bundle_path: &Path,
        plugin_id: &str,
        sample_rate: u32,
        max_block_frames: u32,
    ) -> Result<Self, String> {
        // SAFETY:
        // 隔離子プロセス内で外部 `.clap` モジュールを開き、CLAP ホスト仕様通りに初期化する。
        let entry = unsafe { PluginEntry::load(bundle_path) }
            .map_err(|e| format!("failed to load CLAP entry {}: {e}", bundle_path.display()))?;
        let mut descriptors = describe_clap_bundle(bundle_path)?;
        let mut base_desc = pick_clap_descriptor(&mut descriptors, plugin_id)?;

        let id_cstr = std::ffi::CString::new(base_desc.plugin_id.as_str())
            .map_err(|_| "CLAP plugin_id contains interior NUL byte".to_owned())?;
        let host_info = HostInfo::new("sea-movie-creator", "sea-movie-creator", "", "0.1.0")
            .map_err(|e| format!("failed to construct CLAP HostInfo: {e}"))?;

        let mut instance = PluginInstance::<SeaClapHost>::new(
            |()| SeaClapShared,
            |_| SeaClapMainThread,
            &entry,
            &id_cstr,
            &host_info,
        )
        .map_err(|e| format!("CLAP instantiation failed: {e}"))?;

        let (param_map, params) = collect_clap_params(&mut instance);
        base_desc.parameters = params;

        let config = PluginAudioConfiguration {
            sample_rate: f64::from(sample_rate.max(1)),
            min_frames_count: 1,
            max_frames_count: max_block_frames.max(1),
        };
        let stopped = instance
            .activate(|_, _| (), config)
            .map_err(|e| format!("CLAP activation failed: {e}"))?;
        let started = stopped
            .start_processing()
            .map_err(|e| format!("CLAP start_processing failed: {e}"))?;

        let buf_cap = max_block_frames.max(1) as usize;
        Ok(Self {
            processor: Some(started),
            instance,
            descriptor: base_desc,
            param_map,
            pending_events: EventBuffer::with_capacity(32),
            output_events: EventBuffer::with_capacity(32),
            in_ports: AudioPorts::with_capacity(2, 1),
            out_ports: AudioPorts::with_capacity(2, 1),
            in_l: vec![0.0; buf_cap],
            in_r: vec![0.0; buf_cap],
            out_l: vec![0.0; buf_cap],
            out_r: vec![0.0; buf_cap],
            sample_cursor: 0,
        })
    }

    /// ロードされたプラグインのメタデータを返す。
    #[must_use]
    pub const fn descriptor(&self) -> &AudioPluginDescriptor {
        &self.descriptor
    }

    /// 名前一致するパラメータの値を次回 `process_stereo` 時イベントキューへ積む。
    ///
    /// # Errors
    ///
    /// 未知パラメータ名または不正値の場合エラーを返す。
    pub fn set_parameter(&mut self, name: &str, value: f64) -> Result<f64, String> {
        let param_id = self
            .param_map
            .get(name)
            .copied()
            .ok_or_else(|| format!("unknown CLAP parameter: {name}"))?;
        let event = ParamValueEvent::new(0, param_id, Pckn::match_all(), value);
        self.pending_events.push(&event);
        Ok(value)
    }

    /// インターリーブステレオ入力を `CLAP` へ通してインターリーブ出力を得る。
    ///
    /// # Errors
    ///
    /// フレーム数がバッファ容量超過、またはプラグイン処理失敗時にエラーを返す。
    pub fn process_stereo(
        &mut self,
        interleaved_in: &[f32],
        interleaved_out: &mut [f32],
    ) -> Result<(), String> {
        let frames = interleaved_in.len() / 2;
        if frames == 0 {
            return Ok(());
        }
        if frames > self.in_l.len() || interleaved_out.len() < frames * 2 {
            return Err("audio block size exceeds CLAP configured max_block_frames".to_owned());
        }

        for (idx, pair) in interleaved_in.as_chunks::<2>().0.iter().enumerate() {
            self.in_l[idx] = pair[0];
            self.in_r[idx] = pair[1];
            self.out_l[idx] = 0.0;
            self.out_r[idx] = 0.0;
        }

        let Some(processor) = self.processor.as_mut() else {
            return Err("CLAP processor is not active".to_owned());
        };
        let in_bufs = self.in_ports.with_input_buffers([AudioPortBuffer {
            channels: AudioPortBufferType::f32_input_only([
                InputChannel::variable(&mut self.in_l[..frames]),
                InputChannel::variable(&mut self.in_r[..frames]),
            ]),
            latency: 0,
        }]);
        let mut out_bufs = self.out_ports.with_output_buffers([AudioPortBuffer {
            channels: AudioPortBufferType::f32_output_only([
                &mut self.out_l[..frames],
                &mut self.out_r[..frames],
            ]),
            latency: 0,
        }]);

        let in_events = InputEvents::from_buffer(&self.pending_events);
        self.output_events.clear();
        let mut out_events = OutputEvents::from_buffer(&mut self.output_events);

        processor
            .process(
                &in_bufs,
                &mut out_bufs,
                &in_events,
                &mut out_events,
                Some(self.sample_cursor),
                None,
            )
            .map_err(|e| format!("CLAP process failed: {e}"))?;

        self.pending_events.clear();
        self.sample_cursor = self.sample_cursor.saturating_add(frames as u64);

        for (idx, pair) in interleaved_out
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .enumerate()
        {
            pair[0] = self.out_l[idx];
            pair[1] = self.out_r[idx];
        }
        Ok(())
    }
}

fn pick_clap_descriptor(
    descriptors: &mut Vec<AudioPluginDescriptor>,
    requested_id: &str,
) -> Result<AudioPluginDescriptor, String> {
    if let Some(pos) = descriptors.iter().position(|d| d.plugin_id == requested_id) {
        return Ok(descriptors.swap_remove(pos));
    }
    if let Some(first) = descriptors.first().cloned() {
        return Ok(first);
    }
    Err(format!("CLAP plugin not found in bundle: {requested_id}"))
}

fn collect_clap_params(
    instance: &mut PluginInstance<SeaClapHost>,
) -> (
    BTreeMap<String, ClapId>,
    Vec<AudioPluginParameterDescriptor>,
) {
    let mut map = BTreeMap::new();
    let mut list = Vec::new();
    let handle = instance.plugin_handle();
    let Some(params_ext) = handle.get_extension::<PluginParams>() else {
        return (map, list);
    };
    let count = params_ext.count(&handle);
    let mut info_buf = ParamInfoBuffer::new();
    for idx in 0..count {
        let Some(info) = params_ext.get_info(&handle, idx, &mut info_buf) else {
            continue;
        };
        let id = info.id;
        let display_name = CStr::from_bytes_until_nul(info.name).map_or_else(
            |_| format!("param_{idx}"),
            |s| s.to_string_lossy().into_owned(),
        );
        let name_key = display_name.clone();
        map.insert(name_key.clone(), id);
        list.push(AudioPluginParameterDescriptor {
            name: name_key,
            display_name,
            min_value: info.min_value,
            max_value: info.max_value,
            default_value: info.default_value,
            is_boolean: info.flags.contains(ParamInfoFlags::IS_STEPPED)
                && (info.max_value - info.min_value - 1.0).abs() < 1e-6,
        });
    }
    (map, list)
}
