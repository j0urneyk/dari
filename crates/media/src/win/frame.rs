//! Frames held in Direct3D 11 textures, and copying them to and from memory.

use openh264::formats::YUVSource;
use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};

use crate::codec::CodecError;

/// A frame in an NV12 Direct3D 11 texture (BT.601 limited range), as the capture converts it on
/// the GPU and the hardware encoder reads it without a copy.
#[derive(Clone)]
pub struct NativeFrame {
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl std::fmt::Debug for NativeFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl NativeFrame {
    /// Wraps an NV12 texture with even dimensions. Dari never writes to the texture afterwards.
    pub(super) fn new(texture: ID3D11Texture2D) -> Option<Self> {
        let desc = texture_desc(&texture);
        let usable = desc.Format == DXGI_FORMAT_NV12
            && desc.Width > 0
            && desc.Height > 0
            && desc.Width.is_multiple_of(2)
            && desc.Height.is_multiple_of(2);
        usable.then_some(Self {
            texture,
            width: desc.Width,
            height: desc.Height,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub(super) fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    /// The device the texture belongs to.
    pub(super) fn device(&self) -> Result<ID3D11Device, CodecError> {
        // SAFETY: Querying a texture's device has no preconditions.
        unsafe { self.texture.GetDevice() }.map_err(|error| windows_error("texture device", &error))
    }

    /// Creates an NV12 texture on `device` from I420 planes.
    pub(super) fn from_i420(
        device: &ID3D11Device,
        yuv: &impl YUVSource,
    ) -> Result<Self, CodecError> {
        let (width, height) = yuv.dimensions();
        let unsupported = || CodecError::UnsupportedDimensions {
            width: u32::try_from(width).unwrap_or(u32::MAX),
            height: u32::try_from(height).unwrap_or(u32::MAX),
        };
        let nv12 = i420_to_nv12(yuv);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: u32::try_from(width).map_err(|_| unsupported())?,
            Height: u32::try_from(height).map_err(|_| unsupported())?,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind_flags(),
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        // Planar initial data is one block: the chroma plane follows the luma plane's rows.
        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: nv12.as_ptr().cast(),
            SysMemPitch: desc.Width,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        // SAFETY: `desc` describes an NV12 texture, and `initial` points at `width * height * 3
        // / 2` bytes with rows `width` bytes apart, which outlive the call.
        unsafe {
            device.CreateTexture2D(
                &raw const desc,
                Some(&raw const initial),
                Some(&raw mut texture),
            )
        }
        .map_err(|error| windows_error("texture creation", &error))?;
        texture.and_then(Self::new).ok_or_else(unsupported)
    }

    /// Copies the frame into memory as tightly packed NV12 (the Y plane, then interleaved UV).
    pub(super) fn to_nv12(&self) -> Result<Vec<u8>, CodecError> {
        read_nv12(&self.texture, self.width, self.height)
            .map_err(|error| windows_error("texture readback", &error))
    }

    /// Copies the frame into tightly packed I420 planes (Y, then U, then V).
    pub(crate) fn to_i420(&self) -> Result<Vec<u8>, CodecError> {
        let (width, height) = (self.width as usize, self.height as usize);
        let mut i420 = self.to_nv12()?;
        let (_, chroma) = i420.split_at_mut(width * height);
        let interleaved = chroma.to_vec();
        let (u, v) = chroma.split_at_mut(width * height / 4);
        for ((pair, u), v) in interleaved.as_chunks::<2>().0.iter().zip(u).zip(v) {
            *u = pair[0];
            *v = pair[1];
        }
        Ok(i420)
    }
}

/// Bind flags for NV12 textures: the video processor renders into them.
fn bind_flags() -> u32 {
    D3D11_BIND_RENDER_TARGET.0.cast_unsigned()
}

pub(super) fn texture_desc(texture: &ID3D11Texture2D) -> D3D11_TEXTURE2D_DESC {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: `desc` is a valid out pointer.
    unsafe { texture.GetDesc(&raw mut desc) };
    desc
}

/// Creates an empty NV12 texture the video processor can render into.
pub(super) fn create_nv12_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> windows::core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind_flags(),
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // SAFETY: `desc` is a valid texture description and `texture` a valid out pointer.
    unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut texture)) }?;
    texture.ok_or_else(|| E_POINTER.into())
}

/// Copies an NV12 texture of `width` × `height` into memory with tightly packed rows, through a
/// staging texture.
fn read_nv12(texture: &ID3D11Texture2D, width: u32, height: u32) -> windows::core::Result<Vec<u8>> {
    // SAFETY: Querying a texture's device and the device's context has no preconditions.
    let device = unsafe { texture.GetDevice() }?;
    // SAFETY: As above.
    let context = unsafe { device.GetImmediateContext() }?;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0.cast_unsigned(),
        MiscFlags: 0,
    };
    let mut staging = None;
    // SAFETY: `desc` is a valid texture description and `staging` a valid out pointer.
    unsafe { device.CreateTexture2D(&raw const desc, None, Some(&raw mut staging)) }?;
    let staging: ID3D11Texture2D = staging.ok_or(windows::core::Error::from(E_POINTER))?;
    // SAFETY: Both textures belong to `device` and have the same size and format.
    unsafe { context.CopyResource(&staging, texture) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: The staging texture was created for CPU reads, and `mapped` is a valid out
    // pointer. The texture is unmapped below before it is dropped.
    unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped)) }?;
    // The chroma rows follow the luma rows, each as wide as a luma row.
    let (row_bytes, rows) = (width as usize, height as usize * 3 / 2);
    let pitch = mapped.RowPitch as usize;
    let mut pixels = Vec::with_capacity(row_bytes * rows);
    if pitch >= row_bytes && !mapped.pData.is_null() {
        for row in 0..rows {
            // SAFETY: The mapping holds `rows` rows `pitch` bytes apart, each with at least
            // `row_bytes <= pitch` bytes.
            let source = unsafe {
                std::slice::from_raw_parts(mapped.pData.cast::<u8>().add(row * pitch), row_bytes)
            };
            pixels.extend_from_slice(source);
        }
    }
    // SAFETY: Balances the `Map` above.
    unsafe { context.Unmap(&staging, 0) };
    if pixels.len() == row_bytes * rows {
        Ok(pixels)
    } else {
        Err(E_POINTER.into())
    }
}

/// Interleaves I420 planes into tightly packed NV12.
pub(super) fn i420_to_nv12(yuv: &impl YUVSource) -> Vec<u8> {
    let (width, height) = yuv.dimensions();
    let (y_stride, u_stride, v_stride) = yuv.strides();
    let mut nv12 = Vec::with_capacity(width * height * 3 / 2);
    for row in yuv.y().chunks(y_stride).take(height) {
        nv12.extend_from_slice(&row[..width]);
    }
    let chroma_rows = yuv
        .u()
        .chunks(u_stride)
        .zip(yuv.v().chunks(v_stride))
        .take(height / 2);
    for (u, v) in chroma_rows {
        for (u, v) in u[..width / 2].iter().zip(&v[..width / 2]) {
            nv12.extend_from_slice(&[*u, *v]);
        }
    }
    nv12
}

pub(super) fn windows_error(operation: &'static str, error: &windows::core::Error) -> CodecError {
    CodecError::Windows {
        operation,
        code: error.code().0,
    }
}

#[cfg(test)]
mod tests {
    use openh264::formats::YUVBuffer;

    use super::*;

    #[test]
    fn i420_round_trips_through_an_nv12_texture() {
        let device = super::super::device::create_device().unwrap();
        let (width, height) = (6, 4);
        let i420: Vec<u8> = (0..width * height * 3 / 2)
            .map(|index| u8::try_from(index * 7 % 251).unwrap())
            .collect();
        let frame =
            NativeFrame::from_i420(&device, &YUVBuffer::from_vec(i420.clone(), width, height))
                .unwrap();
        assert_eq!((frame.width(), frame.height()), (6, 4));
        assert_eq!(frame.to_i420().unwrap(), i420);
    }
}
