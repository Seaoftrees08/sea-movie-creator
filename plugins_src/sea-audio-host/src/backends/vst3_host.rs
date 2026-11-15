//! 外部 `.vst3` プラグインの `vst3` (MIT/Apache-2.0 COM ABI) 隔離ホストバックエンド。
#![allow(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::collections::BTreeMap;
use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use libloading::Library;
use sea_export_protocol::audio_ipc::{
    AudioPluginDescriptor, AudioPluginParameterDescriptor, WireAudioPluginFormat,
};
use vst3::ComPtr;
use vst3::Interface;
use vst3::Steinberg::Vst::{
    AudioBusBuffers, AudioBusBuffers__type0, BusDirections_::kInput, BusDirections_::kOutput,
    IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentTrait, IEditController,
    IEditControllerTrait, MediaTypes_::kAudio, ParameterInfo, ProcessData,
    ProcessModes_::kRealtime, ProcessSetup, SpeakerArrangement, SymbolicSampleSizes_::kSample32,
};
use vst3::Steinberg::{
    IPluginBaseTrait, IPluginFactory, IPluginFactoryTrait, PClassInfo, kResultOk,
};

const SPEAKER_STEREO: SpeakerArrangement = 0x3;

/// ロード済み `VST3` プラグインインスタンス（シングルワーカースレッド所有）。
///
/// フィールド宣言順により、COM ポインタ群が解放されたあとに `_library` がアンロードされる。
pub struct Vst3HostedPlugin {
    processor: ComPtr<IAudioProcessor>,
    component: ComPtr<IComponent>,
    edit_controller: Option<ComPtr<IEditController>>,
    _factory: ComPtr<IPluginFactory>,
    _library: Library,
    descriptor: AudioPluginDescriptor,
    param_map: BTreeMap<String, u32>,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    out_l: Vec<f32>,
    out_r: Vec<f32>,
}

impl Drop for Vst3HostedPlugin {
    fn drop(&mut self) {
        // SAFETY:
        // COM インターフェースが有効な状態でのみ停止処理と terminate を逆順で呼び出す。
        unsafe {
            let _ = self.processor.setProcessing(0);
            let _ = self.component.setActive(0);
            let _ = self.component.terminate();
        }
    }
}

/// 指定 `.vst3` バンドルを検査し、含まれるオーディオモジュール記述一覧を返す。
///
/// # Errors
///
/// バイナリ不在・動的ライブラリロード失敗・`GetPluginFactory` 失敗時にエラー文を返す。
pub fn describe_vst3_bundle(bundle_path: &Path) -> Result<Vec<AudioPluginDescriptor>, String> {
    let lib_path = resolve_vst3_library_file(bundle_path)?;
    // SAFETY:
    // 隔離子プロセス内で外部 `.vst3` を読み込み、`GetPluginFactory` のみを呼んで解放する。
    let library = unsafe { Library::new(&lib_path) }
        .map_err(|e| format!("failed to open VST3 module {}: {e}", lib_path.display()))?;
    run_platform_module_init(&library)?;

    let factory = acquire_plugin_factory(&library)?;
    let list = enumerate_vst3_audio_classes(&factory, bundle_path)?;
    drop(factory);
    run_platform_module_exit(&library);
    Ok(list.into_iter().map(|(desc, _)| desc).collect())
}

impl Vst3HostedPlugin {
    /// `.vst3` バンドルから対象プラグインを読み込み、ステレオ処理を開始する。
    ///
    /// # Errors
    ///
    /// ロード・生成・初期化・`setupProcessing` のいずれかに失敗した場合エラー文を返す。
    pub fn load(
        bundle_path: &Path,
        plugin_id: &str,
        sample_rate: u32,
        max_block_frames: u32,
    ) -> Result<Self, String> {
        let lib_path = resolve_vst3_library_file(bundle_path)?;
        // SAFETY:
        // 隔離子プロセス内で `.vst3` モジュールをロードし、`Drop` 時に正しく停止・解放する。
        let library = unsafe { Library::new(&lib_path) }
            .map_err(|e| format!("failed to open VST3 module {}: {e}", lib_path.display()))?;
        run_platform_module_init(&library)?;

        let factory = acquire_plugin_factory(&library)?;
        let classes = enumerate_vst3_audio_classes(&factory, bundle_path)?;
        let (mut descriptor, cid) = pick_vst3_class(&classes, plugin_id)?;

        let component = create_vst3_component(&factory, &cid)?;
        let processor = component
            .cast::<IAudioProcessor>()
            .ok_or_else(|| "VST3 component does not expose IAudioProcessor".to_owned())?;
        let edit_controller = component.cast::<IEditController>();

        // SAFETY:
        // ステレオ標準構成で `IComponent` / `IAudioProcessor` を規定順で初期化・有効化する。
        unsafe {
            if component.initialize(null_mut()) != kResultOk {
                return Err("VST3 IComponent::initialize failed".to_owned());
            }
            let _ = component.activateBus(kAudio, kInput, 0, 1);
            let _ = component.activateBus(kAudio, kOutput, 0, 1);
            let mut in_arr = SPEAKER_STEREO;
            let mut out_arr = SPEAKER_STEREO;
            let _ = processor.setBusArrangements(&raw mut in_arr, 1, &raw mut out_arr, 1);

            let mut setup = ProcessSetup {
                processMode: kRealtime,
                symbolicSampleSize: kSample32,
                maxSamplesPerBlock: i32::try_from(max_block_frames.max(1)).unwrap_or(4096),
                sampleRate: f64::from(sample_rate.max(1)),
            };
            if processor.setupProcessing(&raw mut setup) != kResultOk {
                return Err("VST3 IAudioProcessor::setupProcessing failed".to_owned());
            }
            if component.setActive(1) != kResultOk {
                return Err("VST3 IComponent::setActive(1) failed".to_owned());
            }
            if processor.setProcessing(1) != kResultOk {
                return Err("VST3 IAudioProcessor::setProcessing(1) failed".to_owned());
            }
        }

        let (param_map, param_descs) = edit_controller
            .as_ref()
            .map_or_else(Default::default, collect_vst3_params);
        descriptor.parameters = param_descs;

        let cap = max_block_frames.max(1) as usize;
        Ok(Self {
            processor,
            component,
            edit_controller,
            _factory: factory,
            _library: library,
            descriptor,
            param_map,
            in_l: vec![0.0; cap],
            in_r: vec![0.0; cap],
            out_l: vec![0.0; cap],
            out_r: vec![0.0; cap],
        })
    }

    /// ロードされたプラグインのメタデータを返す。
    #[must_use]
    pub const fn descriptor(&self) -> &AudioPluginDescriptor {
        &self.descriptor
    }

    /// `IEditController` を通じてパラメータ値 (`0.0..=1.0` 正規化) を更新する。
    ///
    /// # Errors
    ///
    /// パラメータ未公開・未知パラメータ名時にエラーを返す。
    pub fn set_parameter(&mut self, name: &str, value: f64) -> Result<f64, String> {
        let Some(ctrl) = self.edit_controller.as_ref() else {
            return Err("VST3 plugin exposes no single-component IEditController".to_owned());
        };
        let param_id = self
            .param_map
            .get(name)
            .copied()
            .ok_or_else(|| format!("unknown VST3 parameter: {name}"))?;
        let norm = value.clamp(0.0, 1.0);
        // SAFETY: `ctrl` は有効な `ComPtr<IEditController>`。
        let res = unsafe { ctrl.setParamNormalized(param_id, norm) };
        if res == kResultOk {
            Ok(norm)
        } else {
            Err(format!("VST3 setParamNormalized failed: {res}"))
        }
    }

    /// インターリーブステレオ入力を `VST3` プロセッサで処理する。
    ///
    /// # Errors
    ///
    /// フレーム数超過または `IAudioProcessor::process` 失敗時にエラーを返す。
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
            return Err("audio block size exceeds VST3 configured maxSamplesPerBlock".to_owned());
        }

        for (idx, pair) in interleaved_in.as_chunks::<2>().0.iter().enumerate() {
            self.in_l[idx] = pair[0];
            self.in_r[idx] = pair[1];
            self.out_l[idx] = 0.0;
            self.out_r[idx] = 0.0;
        }

        let mut in_ptrs = [self.in_l.as_mut_ptr(), self.in_r.as_mut_ptr()];
        let mut out_ptrs = [self.out_l.as_mut_ptr(), self.out_r.as_mut_ptr()];
        let mut in_bus = AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: in_ptrs.as_mut_ptr(),
            },
        };
        let mut out_bus = AudioBusBuffers {
            numChannels: 2,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: out_ptrs.as_mut_ptr(),
            },
        };

        let mut data = ProcessData {
            processMode: kRealtime,
            symbolicSampleSize: kSample32,
            numSamples: frames as i32,
            numInputs: 1,
            numOutputs: 1,
            inputs: &raw mut in_bus,
            outputs: &raw mut out_bus,
            inputParameterChanges: null_mut(),
            outputParameterChanges: null_mut(),
            inputEvents: null_mut(),
            outputEvents: null_mut(),
            processContext: null_mut(),
        };

        // SAFETY: `in_bus`, `out_bus`, `in_ptrs`, `out_ptrs` はこの呼び出しの間有効かつ非エイリアス。
        let res = unsafe { self.processor.process(&raw mut data) };
        if res != kResultOk {
            return Err(format!("VST3 IAudioProcessor::process failed: {res}"));
        }

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

fn resolve_vst3_library_file(bundle_path: &Path) -> Result<PathBuf, String> {
    if bundle_path.is_file() {
        return Ok(bundle_path.to_path_buf());
    }
    let contents = bundle_path.join("Contents");
    for sub in ["x86_64-win", "x86_64-linux", "MacOS"] {
        let dir = contents.join(sub);
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(format!(
        "VST3 bundle contains no dynamic library: {}",
        bundle_path.display()
    ))
}

fn acquire_plugin_factory(library: &Library) -> Result<ComPtr<IPluginFactory>, String> {
    type GetFactoryFn = unsafe extern "system" fn() -> *mut IPluginFactory;
    // SAFETY: `GetPluginFactory` は VST3 ABI 標準の工場取得シンボル。
    let func = unsafe { library.get::<GetFactoryFn>(b"GetPluginFactory\0") }
        .map_err(|e| format!("missing GetPluginFactory symbol: {e}"))?;
    // SAFETY: `func()` は所有参照カウント付きの `IPluginFactory*` を返す。
    let raw = unsafe { func() };
    // SAFETY: 非 NULL チェックおよび所有権移譲。
    unsafe { ComPtr::from_raw(raw) }.ok_or_else(|| "GetPluginFactory returned null".to_owned())
}

fn create_vst3_component(
    factory: &ComPtr<IPluginFactory>,
    cid: &[i8; 16],
) -> Result<ComPtr<IComponent>, String> {
    let mut raw: *mut c_void = null_mut();
    let iid = <IComponent as Interface>::IID;
    // SAFETY: `factory` は有効であり、`cid` と `iid` のポインタ有効期間はこの呼び出し中に限る。
    let res = unsafe {
        factory.createInstance(
            cid.as_ptr().cast::<c_char>(),
            iid.as_ptr().cast::<c_char>(),
            &raw mut raw,
        )
    };
    if res != kResultOk || raw.is_null() {
        return Err(format!("VST3 createInstance(IComponent) failed: {res}"));
    }
    // SAFETY: `createInstance` 成功時は `IComponent` COM ポインタが返る。
    unsafe { ComPtr::from_raw(raw.cast::<IComponent>()) }
        .ok_or_else(|| "VST3 createInstance returned null pointer".to_owned())
}

fn enumerate_vst3_audio_classes(
    factory: &ComPtr<IPluginFactory>,
    bundle_path: &Path,
) -> Result<Vec<(AudioPluginDescriptor, [i8; 16])>, String> {
    // SAFETY: `factory` は有効。
    let count = unsafe { factory.countClasses() };
    if count <= 0 {
        return Err("VST3 factory reports zero classes".to_owned());
    }
    let mut results = Vec::new();
    for idx in 0..count {
        // SAFETY: `PClassInfo` は C 互換 POD 構造体。
        let mut info: PClassInfo = unsafe { std::mem::zeroed() };
        // SAFETY: `idx < count` の範囲で呼び出す。
        if unsafe { factory.getClassInfo(idx, &raw mut info) } != kResultOk {
            continue;
        }
        let category = c_char_buf_to_string(&info.category);
        if category != "Audio Module Class" {
            continue;
        }
        let name = c_char_buf_to_string(&info.name);
        let id_hex = encode_tuid_hex(&info.cid);
        results.push((
            AudioPluginDescriptor {
                plugin_id: id_hex,
                name,
                vendor: "VST3 Module".to_owned(),
                format: WireAudioPluginFormat::Vst3,
                bundle_path: Some(bundle_path.to_string_lossy().into_owned()),
                parameters: Vec::new(),
            },
            info.cid,
        ));
    }
    Ok(results)
}

fn pick_vst3_class(
    classes: &[(AudioPluginDescriptor, [i8; 16])],
    requested_id: &str,
) -> Result<(AudioPluginDescriptor, [i8; 16]), String> {
    if let Some(found) = classes.iter().find(|(d, _)| d.plugin_id == requested_id) {
        return Ok(found.clone());
    }
    if let Some(first) = classes.first() {
        return Ok(first.clone());
    }
    Err(format!(
        "no VST3 Audio Module Class found for {requested_id}"
    ))
}

fn collect_vst3_params(
    ctrl: &ComPtr<IEditController>,
) -> (BTreeMap<String, u32>, Vec<AudioPluginParameterDescriptor>) {
    let mut map = BTreeMap::new();
    let mut descs = Vec::new();
    // SAFETY: `ctrl` は有効。
    let count = unsafe { ctrl.getParameterCount() };
    for idx in 0..count {
        // SAFETY: `ParameterInfo` は C 互換 POD 構造体。
        let mut info: ParameterInfo = unsafe { std::mem::zeroed() };
        // SAFETY: `idx < count` の範囲で呼び出す。
        if unsafe { ctrl.getParameterInfo(idx, &raw mut info) } != kResultOk {
            continue;
        }
        let title = utf16_tchar_to_string(&info.title);
        if title.is_empty() {
            continue;
        }
        map.insert(title.clone(), info.id);
        descs.push(AudioPluginParameterDescriptor {
            name: title.clone(),
            display_name: title,
            min_value: 0.0,
            max_value: 1.0,
            default_value: info.defaultNormalizedValue,
            is_boolean: info.stepCount == 1,
        });
    }
    (map, descs)
}

fn c_char_buf_to_string(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().map(|&c| c as u8).collect();
    CStr::from_bytes_until_nul(&bytes)
        .map_or_else(|_| String::new(), |c| c.to_string_lossy().into_owned())
}

fn utf16_tchar_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn encode_tuid_hex(cid: &[i8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for &b in cid {
        use std::fmt::Write as _;
        let _ = write!(&mut s, "{:02X}", b as u8);
    }
    s
}

#[cfg(target_os = "windows")]
fn run_platform_module_init(library: &Library) -> Result<(), String> {
    type InitFn = unsafe extern "system" fn() -> bool;
    // SAFETY: 任意シンボル `InitDll` が存在する場合のみ呼び出す。
    if let Ok(init_fn) = unsafe { library.get::<InitFn>(b"InitDll\0") }
        && !unsafe { init_fn() }
    {
        return Err("VST3 InitDll returned false".to_owned());
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn run_platform_module_init(library: &Library) -> Result<(), String> {
    type EntryFn = unsafe extern "C" fn(*mut c_void) -> bool;
    let sym = if cfg!(target_os = "macos") {
        b"bundleEntry\0".as_slice()
    } else {
        b"ModuleEntry\0".as_slice()
    };
    // SAFETY: プラットフォーム標準エントリーが存在する場合のみ NULL/ハンドル相当を適切に渡す。
    if let Ok(entry) = unsafe { library.get::<EntryFn>(sym) }
        && !unsafe { entry(null_mut()) }
    {
        return Err("VST3 module entry returned false".to_owned());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn run_platform_module_exit(library: &Library) {
    type ExitFn = unsafe extern "system" fn() -> bool;
    // SAFETY: `InitDll` 対応シンボルがあれば終了時に呼ぶ。
    if let Ok(exit_fn) = unsafe { library.get::<ExitFn>(b"ExitDll\0") } {
        let _ = unsafe { exit_fn() };
    }
}

#[cfg(not(target_os = "windows"))]
fn run_platform_module_exit(library: &Library) {
    type ExitFn = unsafe extern "C" fn() -> bool;
    let sym = if cfg!(target_os = "macos") {
        b"bundleExit\0".as_slice()
    } else {
        b"ModuleExit\0".as_slice()
    };
    // SAFETY: 対応終了シンボルがあれば呼ぶ。
    if let Ok(exit_fn) = unsafe { library.get::<ExitFn>(sym) } {
        let _ = unsafe { exit_fn() };
    }
}
