//! GPU テクスチャプール（素材フレームキャッシュおよびピンポン／ダウンスケール描画ターゲット管理）。

use std::collections::{HashMap, VecDeque};

use crate::core::ids::AssetId;
use crate::media::decoder::VideoFrameBuffer;

const GPU_TEXTURE_BUDGET_BYTES: usize = 128 * 1024 * 1024;

/// GPU テクスチャキャッシュの識別キー。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GpuTextureKey {
    /// 論理素材 ID（プレースホルダ用 ID を含む）。
    pub asset_id: AssetId,
    /// フレーム番号（静止画・固定画像は `0`、字幕等はコンテンツハッシュ等を割り当てる）。
    pub frame_index: u64,
    /// ピクセル幅。
    pub width: u32,
    /// ピクセル高さ。
    pub height: u32,
}

struct CachedGpuTexture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    byte_size: usize,
}

/// レンダーターゲット（フル解像度ピンポンペア + プレビュー出力サイズ）保持構造体。
pub(crate) struct CompositorTargets {
    pub project_width: u32,
    pub project_height: u32,
    pub ping: (wgpu::Texture, wgpu::TextureView),
    pub pong: (wgpu::Texture, wgpu::TextureView),
    pub output_width: u32,
    pub output_height: u32,
    pub output: (wgpu::Texture, wgpu::TextureView),
}

impl CompositorTargets {
    pub(crate) fn new(
        device: &wgpu::Device,
        project_width: u32,
        project_height: u32,
        output_width: u32,
        output_height: u32,
    ) -> Self {
        let pw = project_width.max(1);
        let ph = project_height.max(1);
        let ow = output_width.max(1);
        let oh = output_height.max(1);
        Self {
            project_width: pw,
            project_height: ph,
            ping: create_render_target(device, pw, ph, "sea_comp_ping"),
            pong: create_render_target(device, pw, ph, "sea_comp_pong"),
            output_width: ow,
            output_height: oh,
            output: create_render_target(device, ow, oh, "sea_comp_out"),
        }
    }

    pub(crate) fn ensure_sizes(
        &mut self,
        device: &wgpu::Device,
        project_width: u32,
        project_height: u32,
        output_width: u32,
        output_height: u32,
    ) {
        let pw = project_width.max(1);
        let ph = project_height.max(1);
        if self.project_width != pw || self.project_height != ph {
            self.project_width = pw;
            self.project_height = ph;
            self.ping = create_render_target(device, pw, ph, "sea_comp_ping");
            self.pong = create_render_target(device, pw, ph, "sea_comp_pong");
        }
        let ow = output_width.max(1);
        let oh = output_height.max(1);
        if self.output_width != ow || self.output_height != oh {
            self.output_width = ow;
            self.output_height = oh;
            self.output = create_render_target(device, ow, oh, "sea_comp_out");
        }
    }
}

/// GPU 側アップロード済み素材フレームの LRU テクスチャプール。
pub struct GpuTexturePool {
    budget_bytes: usize,
    used_bytes: usize,
    entries: HashMap<GpuTextureKey, CachedGpuTexture>,
    order: VecDeque<GpuTextureKey>,
}

impl GpuTexturePool {
    /// 既定容量（128 MiB）の GPU テクスチャプールを作成する。
    #[must_use]
    pub fn new() -> Self {
        Self {
            budget_bytes: GPU_TEXTURE_BUDGET_BYTES,
            used_bytes: 0,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// キャッシュ中のテクスチャ総バイト数を返す。
    #[must_use]
    pub const fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    /// 登録済みテクスチャ数を返す。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// キャッシュが空かどうかを返す。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 全テクスチャキャッシュを破棄する。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.used_bytes = 0;
    }

    /// キーに一致するテクスチャを取得し、未登録であれば CPU バッファを GPU へ転送して登録する。
    pub(crate) fn get_or_upload_bgra(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        key: &GpuTextureKey,
        buffer: &VideoFrameBuffer,
    ) -> &wgpu::TextureView {
        if self.entries.contains_key(key) {
            self.touch_key(key);
            return &self.entries[key].view;
        }

        let w = buffer.width.max(1);
        let h = buffer.height.max(1);
        let bgra_owned;
        let pixels = if buffer.format == crate::media::decoder::FramePixelFormat::Bgra8 {
            &buffer.data
        } else {
            bgra_owned = buffer.to_bgra8();
            &bgra_owned
        };

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sea_layer_tex"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w.saturating_mul(4)),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let byte_size = usize::try_from(w)
            .unwrap_or(1)
            .saturating_mul(usize::try_from(h).unwrap_or(1))
            .saturating_mul(4);

        self.used_bytes = self.used_bytes.saturating_add(byte_size);
        self.order.push_back(key.clone());
        self.entries.insert(
            key.clone(),
            CachedGpuTexture {
                texture,
                view,
                byte_size,
            },
        );
        self.evict_over_budget(key);
        &self.entries[key].view
    }

    /// 1x1 透過クリア等の内部参照テクスチャを生成する。
    pub(crate) fn create_solid_texture_view(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        bgra: [u8; 4],
    ) -> (wgpu::Texture, wgpu::TextureView) {
        let w = width.max(1);
        let h = height.max(1);
        let pixel_count = usize::try_from(w)
            .unwrap_or(1)
            .saturating_mul(usize::try_from(h).unwrap_or(1));
        let mut bytes = Vec::with_capacity(pixel_count.saturating_mul(4));
        for _ in 0..pixel_count {
            bytes.extend_from_slice(&bgra);
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sea_solid_tex"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w.saturating_mul(4)),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }

    fn touch_key(&mut self, key: &GpuTextureKey) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.clone());
    }

    fn evict_over_budget(&mut self, keep_key: &GpuTextureKey) {
        while self.used_bytes > self.budget_bytes && self.entries.len() > 1 {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if &oldest == keep_key {
                self.order.push_back(oldest);
                continue;
            }
            if let Some(removed) = self.entries.remove(&oldest) {
                drop(removed.texture);
                self.used_bytes = self.used_bytes.saturating_sub(removed.byte_size);
            }
        }
    }
}

impl Default for GpuTexturePool {
    fn default() -> Self {
        Self::new()
    }
}

fn create_render_target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    label: &str,
) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    (texture, view)
}
