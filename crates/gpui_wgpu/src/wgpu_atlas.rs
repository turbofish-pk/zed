use anyhow::{Context as _, Result};
use etagere::{size2, BucketedAtlasAllocator};
use gpui::{
  AtlasBackend, AtlasKey, AtlasState, AtlasTextureId, AtlasTextureKind, AtlasTextureList,
  AtlasTile, Bounds, DevicePixels, PlatformAtlas, Point, Size,
};
use parking_lot::Mutex;
use std::{borrow::Cow, ops, sync::Arc};

use crate::WgpuContext;

fn device_size_to_etagere(size: Size<DevicePixels>) -> etagere::Size {
  size2(size.width.0, size.height.0)
}

fn etagere_point_to_device(point: etagere::Point) -> Point<DevicePixels> {
  Point {
    x: DevicePixels(point.x),
    y: DevicePixels(point.y),
  }
}

pub struct WgpuAtlas(Mutex<AtlasState<WgpuAtlasTextures>>);

struct PendingUpload {
  id: AtlasTextureId,
  bounds: Bounds<DevicePixels>,
  data: Vec<u8>,
}

struct WgpuAtlasTextures {
  device: Arc<wgpu::Device>,
  queue: Arc<wgpu::Queue>,
  max_texture_size: u32,
  color_texture_format: wgpu::TextureFormat,
  storage: WgpuAtlasStorage,
  pending_uploads: Vec<PendingUpload>,
  next_texture_generation: u64,
}

pub struct WgpuTextureInfo {
  pub view: wgpu::TextureView,
  /// Distinguishes new textures that reuse an [`AtlasTextureId`].
  pub generation: u64,
}

impl WgpuAtlas {
  pub fn new(
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    color_texture_format: wgpu::TextureFormat,
  ) -> Self {
    let max_texture_size = device.limits().max_texture_dimension_2d;
    WgpuAtlas(Mutex::new(AtlasState::new(WgpuAtlasTextures {
      device,
      queue,
      max_texture_size,
      color_texture_format,
      storage: WgpuAtlasStorage::default(),
      pending_uploads: Vec::new(),
      next_texture_generation: 0,
    })))
  }

  pub fn from_context(context: &WgpuContext) -> Self {
    Self::new(
      context.device.clone(),
      context.queue.clone(),
      context.color_texture_format(),
    )
  }

  pub fn before_frame(&self) {
    let mut lock = self.0.lock();
    lock.backend.flush_uploads();
  }

  /// Returns the view backing `id`, or `None` once every tile in it has been
  /// removed. A scene can still reference such a texture when a cached view
  /// replays a paint from before the image was dropped, so callers must skip
  /// those sprites rather than assume the texture exists.
  pub fn get_texture_info(&self, id: AtlasTextureId) -> Option<WgpuTextureInfo> {
    let lock = self.0.lock();
    let texture = lock.backend.storage.get(id)?;
    Some(WgpuTextureInfo {
      view: texture.view.clone(),
      generation: texture.generation,
    })
  }

  /// Clears all cached textures and tiles, forcing them to be recreated.
  /// Use this for incremental recovery when the device is still valid.
  pub fn clear(&self) {
    self.0.lock().clear(|textures| {
      textures.storage = WgpuAtlasStorage::default();
      textures.pending_uploads.clear();
    });
  }

  /// Handles device lost by clearing all textures and cached tiles.
  /// The atlas will lazily recreate textures as needed on subsequent frames.
  pub fn handle_device_lost(&self, context: &WgpuContext) {
    self.0.lock().clear(|textures| {
      textures.device = context.device.clone();
      textures.queue = context.queue.clone();
      textures.color_texture_format = context.color_texture_format();
      textures.storage = WgpuAtlasStorage::default();
      textures.pending_uploads.clear();
    });
  }
}

impl PlatformAtlas for WgpuAtlas {
  fn get_or_insert_with<'a>(
    &self,
    key: AtlasKey,
    build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
  ) -> Result<Option<AtlasTile>> {
    self.0.lock().get_or_insert_with(key, build)
  }

  fn remove(&self, key: &AtlasKey) {
    self.0.lock().remove(key);
  }
}

impl AtlasBackend for WgpuAtlasTextures {
  fn insert(
    &mut self,
    kind: AtlasTextureKind,
    size: Size<DevicePixels>,
    bytes: &[u8],
  ) -> Result<AtlasTile> {
    let tile = self.allocate(size, kind).context("failed to allocate")?;
    self.upload_texture(tile.texture_id, tile.bounds, bytes);
    Ok(tile)
  }

  fn remove(&mut self, tile: AtlasTile) {
    let id = tile.texture_id;
    let Some(texture_slot) = self.storage[id.kind].textures.get_mut(id.index as usize) else {
      return;
    };

    if let Some(mut texture) = texture_slot.take() {
      texture.allocator.deallocate(tile.tile_id.into());
      texture.decrement_ref_count();
      if texture.is_unreferenced() {
        self
          .pending_uploads
          .retain(|upload| upload.id != texture.id);
        self.storage[id.kind]
          .free_list
          .push(texture.id.index as usize);
      } else {
        *texture_slot = Some(texture);
      }
    }
  }
}

impl WgpuAtlasTextures {
  fn allocate(
    &mut self,
    size: Size<DevicePixels>,
    texture_kind: AtlasTextureKind,
  ) -> Option<AtlasTile> {
    {
      let textures = &mut self.storage[texture_kind];

      if let Some(tile) = textures
        .iter_mut()
        .rev()
        .find_map(|texture| texture.allocate(size))
      {
        return Some(tile);
      }
    }

    let texture = self.push_texture(size, texture_kind);
    texture.allocate(size)
  }

  fn push_texture(
    &mut self,
    min_size: Size<DevicePixels>,
    kind: AtlasTextureKind,
  ) -> &mut WgpuAtlasTexture {
    const DEFAULT_ATLAS_SIZE: Size<DevicePixels> = Size {
      width: DevicePixels(1024),
      height: DevicePixels(1024),
    };
    let max_texture_size = self.max_texture_size as i32;
    let max_atlas_size = Size {
      width: DevicePixels(max_texture_size),
      height: DevicePixels(max_texture_size),
    };

    let size = min_size.min(&max_atlas_size).max(&DEFAULT_ATLAS_SIZE);
    let format = match kind {
      AtlasTextureKind::Monochrome => wgpu::TextureFormat::R8Unorm,
      AtlasTextureKind::Subpixel | AtlasTextureKind::Polychrome => self.color_texture_format,
    };

    let texture = self.device.create_texture(&wgpu::TextureDescriptor {
      label: Some("atlas"),
      size: wgpu::Extent3d {
        width: size.width.0 as u32,
        height: size.height.0 as u32,
        depth_or_array_layers: 1,
      },
      mip_level_count: 1,
      sample_count: 1,
      dimension: wgpu::TextureDimension::D2,
      format,
      usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
      view_formats: &[],
    });

    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let texture_list = &mut self.storage[kind];
    let index = texture_list.free_list.pop();
    let generation = self.next_texture_generation;
    self.next_texture_generation = self.next_texture_generation.wrapping_add(1);

    let atlas_texture = WgpuAtlasTexture {
      id: AtlasTextureId {
        index: index.unwrap_or(texture_list.textures.len()) as u32,
        kind,
      },
      generation,
      allocator: BucketedAtlasAllocator::new(device_size_to_etagere(size)),
      format,
      texture,
      view,
      live_atlas_keys: 0,
    };

    if let Some(ix) = index {
      texture_list.textures[ix] = Some(atlas_texture);
      texture_list
        .textures
        .get_mut(ix)
        .and_then(|t| t.as_mut())
        .expect("texture must exist")
    } else {
      texture_list.textures.push(Some(atlas_texture));
      texture_list
        .textures
        .last_mut()
        .and_then(|t| t.as_mut())
        .expect("texture must exist")
    }
  }

  fn upload_texture(&mut self, id: AtlasTextureId, bounds: Bounds<DevicePixels>, bytes: &[u8]) {
    let data = self
      .storage
      .get(id)
      .map(|texture| swizzle_upload_data(bytes, texture.format))
      .unwrap_or_else(|| bytes.to_vec());

    self
      .pending_uploads
      .push(PendingUpload { id, bounds, data });
  }

  fn flush_uploads(&mut self) {
    for upload in self.pending_uploads.drain(..) {
      let Some(texture) = self.storage.get(upload.id) else {
        continue;
      };
      let bytes_per_pixel = texture.bytes_per_pixel();

      self.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
          texture: &texture.texture,
          mip_level: 0,
          origin: wgpu::Origin3d {
            x: upload.bounds.origin.x.0 as u32,
            y: upload.bounds.origin.y.0 as u32,
            z: 0,
          },
          aspect: wgpu::TextureAspect::All,
        },
        &upload.data,
        wgpu::TexelCopyBufferLayout {
          offset: 0,
          bytes_per_row: Some(upload.bounds.size.width.0 as u32 * bytes_per_pixel as u32),
          rows_per_image: None,
        },
        wgpu::Extent3d {
          width: upload.bounds.size.width.0 as u32,
          height: upload.bounds.size.height.0 as u32,
          depth_or_array_layers: 1,
        },
      );
    }
  }
}

#[derive(Default)]
struct WgpuAtlasStorage {
  monochrome_textures: AtlasTextureList<WgpuAtlasTexture>,
  subpixel_textures: AtlasTextureList<WgpuAtlasTexture>,
  polychrome_textures: AtlasTextureList<WgpuAtlasTexture>,
}

impl ops::Index<AtlasTextureKind> for WgpuAtlasStorage {
  type Output = AtlasTextureList<WgpuAtlasTexture>;
  fn index(&self, kind: AtlasTextureKind) -> &Self::Output {
    match kind {
      AtlasTextureKind::Monochrome => &self.monochrome_textures,
      AtlasTextureKind::Subpixel => &self.subpixel_textures,
      AtlasTextureKind::Polychrome => &self.polychrome_textures,
    }
  }
}

impl ops::IndexMut<AtlasTextureKind> for WgpuAtlasStorage {
  fn index_mut(&mut self, kind: AtlasTextureKind) -> &mut Self::Output {
    match kind {
      AtlasTextureKind::Monochrome => &mut self.monochrome_textures,
      AtlasTextureKind::Subpixel => &mut self.subpixel_textures,
      AtlasTextureKind::Polychrome => &mut self.polychrome_textures,
    }
  }
}

impl WgpuAtlasStorage {
  fn get(&self, id: AtlasTextureId) -> Option<&WgpuAtlasTexture> {
    self[id.kind]
      .textures
      .get(id.index as usize)
      .and_then(|t| t.as_ref())
  }
}

struct WgpuAtlasTexture {
  id: AtlasTextureId,
  generation: u64,
  allocator: BucketedAtlasAllocator,
  texture: wgpu::Texture,
  view: wgpu::TextureView,
  format: wgpu::TextureFormat,
  live_atlas_keys: u32,
}

impl WgpuAtlasTexture {
  fn allocate(&mut self, size: Size<DevicePixels>) -> Option<AtlasTile> {
    let allocation = self.allocator.allocate(device_size_to_etagere(size))?;
    let tile = AtlasTile {
      texture_id: self.id,
      tile_id: allocation.id.into(),
      padding: 0,
      bounds: Bounds {
        origin: etagere_point_to_device(allocation.rectangle.min),
        size,
      },
    };
    self.live_atlas_keys += 1;
    Some(tile)
  }

  fn bytes_per_pixel(&self) -> u8 {
    match self.format {
      wgpu::TextureFormat::R8Unorm => 1,
      wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm => 4,
      _ => 4,
    }
  }

  fn decrement_ref_count(&mut self) {
    self.live_atlas_keys -= 1;
  }

  fn is_unreferenced(&self) -> bool {
    self.live_atlas_keys == 0
  }
}

fn swizzle_upload_data(bytes: &[u8], format: wgpu::TextureFormat) -> Vec<u8> {
  match format {
    wgpu::TextureFormat::Rgba8Unorm => {
      let mut data = bytes.to_vec();
      for pixel in data.chunks_exact_mut(4) {
        pixel.swap(0, 2);
      }
      data
    }
    _ => bytes.to_vec(),
  }
}
