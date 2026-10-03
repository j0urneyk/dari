//! Frames held in CoreVideo pixel buffers.

use std::ptr::NonNull;

use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVReturnSuccess,
};
use openh264::formats::YUVSource;

use crate::codec::CodecError;

/// The pixel format of every native frame: NV12 (`420v`), BT.601 limited range, which is what
/// the H.264 stream carries and what OpenH264 decodes to.
pub(crate) const NATIVE_PIXEL_FORMAT: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;

/// A frame in an IOSurface-backed NV12 pixel buffer, as ScreenCaptureKit delivers it and
/// VideoToolbox encodes it without a copy.
#[derive(Clone)]
pub struct NativeFrame {
    buffer: CFRetained<CVPixelBuffer>,
    width: u32,
    height: u32,
}

// SAFETY: CoreVideo retains and releases pixel buffers atomically, and Dari never writes to a
// buffer after handing it on: captured buffers are only read, and converted buffers are filled
// before the `NativeFrame` exists.
unsafe impl Send for NativeFrame {}

impl std::fmt::Debug for NativeFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl NativeFrame {
    /// Wraps an NV12 pixel buffer with even dimensions.
    pub(crate) fn new(buffer: CFRetained<CVPixelBuffer>) -> Option<Self> {
        let width = u32::try_from(CVPixelBufferGetWidth(&buffer)).ok()?;
        let height = u32::try_from(CVPixelBufferGetHeight(&buffer)).ok()?;
        let usable = CVPixelBufferGetPixelFormatType(&buffer) == NATIVE_PIXEL_FORMAT
            && width > 0
            && height > 0
            && width % 2 == 0
            && height % 2 == 0;
        usable.then_some(Self {
            buffer,
            width,
            height,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub(crate) fn buffer(&self) -> &CVPixelBuffer {
        &self.buffer
    }

    /// Creates an NV12 frame from I420 planes.
    pub(crate) fn from_i420(yuv: &impl YUVSource) -> Result<Self, CodecError> {
        let (width, height) = yuv.dimensions();
        let empty = CFDictionary::<CFString, CFType>::empty();
        // SAFETY: Reading an immutable static CoreVideo constant.
        let surface_key = unsafe { kCVPixelBufferIOSurfacePropertiesKey };
        // An empty IOSurface properties dictionary asks for an IOSurface-backed buffer, the kind
        // the hardware encoder reads without a copy.
        let attributes =
            CFDictionary::<CFString, CFType>::from_slices(&[surface_key], &[empty.as_ref()]);
        let mut raw = std::ptr::null_mut();
        // SAFETY: The attributes dictionary maps CFString keys to CF values as required, and
        // `raw` is a valid out pointer.
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width,
                height,
                NATIVE_PIXEL_FORMAT,
                Some(attributes.as_opaque()),
                NonNull::from(&mut raw),
            )
        };
        let buffer = NonNull::new(raw)
            .filter(|_| status == kCVReturnSuccess)
            .ok_or(CodecError::VideoToolbox {
                operation: "pixel buffer creation",
                status,
            })?;
        // SAFETY: `CVPixelBufferCreate` returned a +1 reference.
        let buffer = unsafe { CFRetained::from_raw(buffer) };
        {
            let locked = LockedPlanes::lock(&buffer, CVPixelBufferLockFlags::empty())?;
            let (y_stride, u_stride, v_stride) = yuv.strides();
            let mut luma = locked.plane(0)?;
            for (row, target) in yuv.y().chunks(y_stride).take(height).zip(luma.rows_mut()) {
                target[..width].copy_from_slice(&row[..width]);
            }
            let mut chroma = locked.plane(1)?;
            let chroma_rows = yuv
                .u()
                .chunks(u_stride)
                .zip(yuv.v().chunks(v_stride))
                .take(height / 2);
            for ((u, v), target) in chroma_rows.zip(chroma.rows_mut()) {
                for (pair, (u, v)) in target[..width]
                    .as_chunks_mut::<2>()
                    .0
                    .iter_mut()
                    .zip(u.iter().zip(v))
                {
                    *pair = [*u, *v];
                }
            }
        }
        Self::new(buffer).ok_or(CodecError::UnsupportedDimensions {
            width: u32::try_from(width).unwrap_or(u32::MAX),
            height: u32::try_from(height).unwrap_or(u32::MAX),
        })
    }

    /// Copies the frame into tightly packed I420 planes (Y, then U, then V).
    pub(crate) fn to_i420(&self) -> Result<Vec<u8>, CodecError> {
        let (width, height) = (self.width as usize, self.height as usize);
        let mut i420 = vec![0u8; width * height * 3 / 2];
        let (luma_out, chroma_out) = i420.split_at_mut(width * height);
        let (u_out, v_out) = chroma_out.split_at_mut(width * height / 4);
        let locked = LockedPlanes::lock(&self.buffer, CVPixelBufferLockFlags::ReadOnly)?;
        for (row, target) in locked.plane(0)?.rows().zip(luma_out.chunks_mut(width)) {
            target.copy_from_slice(&row[..width]);
        }
        let chroma = locked.plane(1)?;
        let chroma_rows = chroma
            .rows()
            .zip(u_out.chunks_mut(width / 2).zip(v_out.chunks_mut(width / 2)));
        for (row, (u, v)) in chroma_rows {
            for ((pair, u), v) in row[..width].as_chunks::<2>().0.iter().zip(u).zip(v) {
                *u = pair[0];
                *v = pair[1];
            }
        }
        Ok(i420)
    }
}

/// A pixel buffer with its base address locked for CPU access; unlocked on drop.
struct LockedPlanes<'a> {
    buffer: &'a CVPixelBuffer,
    flags: CVPixelBufferLockFlags,
}

/// One plane of a locked pixel buffer.
struct Plane<'a> {
    base: NonNull<u8>,
    stride: usize,
    row_bytes: usize,
    rows: usize,
    _locked: std::marker::PhantomData<&'a ()>,
}

impl<'a> LockedPlanes<'a> {
    fn lock(buffer: &'a CVPixelBuffer, flags: CVPixelBufferLockFlags) -> Result<Self, CodecError> {
        // SAFETY: `buffer` is a valid pixel buffer; the lock is released in `Drop`.
        let status = unsafe { CVPixelBufferLockBaseAddress(buffer, flags) };
        if status != kCVReturnSuccess {
            return Err(CodecError::VideoToolbox {
                operation: "pixel buffer lock",
                status,
            });
        }
        Ok(Self { buffer, flags })
    }

    /// Plane 0 is luma (full size); plane 1 is interleaved chroma (half height, full row width).
    fn plane(&self, index: usize) -> Result<Plane<'_>, CodecError> {
        let height = CVPixelBufferGetHeight(self.buffer);
        let base = NonNull::new(CVPixelBufferGetBaseAddressOfPlane(self.buffer, index).cast())
            .ok_or(CodecError::MalformedOutput("pixel buffer plane"))?;
        let stride = CVPixelBufferGetBytesPerRowOfPlane(self.buffer, index);
        let row_bytes = CVPixelBufferGetWidth(self.buffer);
        if row_bytes > stride {
            return Err(CodecError::MalformedOutput("pixel buffer stride"));
        }
        Ok(Plane {
            base,
            stride,
            row_bytes,
            rows: if index == 0 { height } else { height / 2 },
            _locked: std::marker::PhantomData,
        })
    }
}

impl Drop for LockedPlanes<'_> {
    fn drop(&mut self) {
        // SAFETY: Balances the lock taken in `lock` with the same flags.
        let _status = unsafe { CVPixelBufferUnlockBaseAddress(self.buffer, self.flags) };
    }
}

impl Plane<'_> {
    fn rows(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.rows).map(|row| {
            // SAFETY: The buffer is locked, and each row starts `stride` bytes after the last
            // and holds `row_bytes <= stride` bytes inside the plane.
            unsafe {
                std::slice::from_raw_parts(
                    self.base.as_ptr().add(row * self.stride),
                    self.row_bytes,
                )
            }
        })
    }

    fn rows_mut(&mut self) -> impl Iterator<Item = &mut [u8]> {
        (0..self.rows).map(|row| {
            // SAFETY: As in `rows`; the buffer was locked for writing, and the rows are disjoint
            // regions each handed out once.
            unsafe {
                std::slice::from_raw_parts_mut(
                    self.base.as_ptr().add(row * self.stride),
                    self.row_bytes,
                )
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use openh264::formats::YUVBuffer;

    use super::*;

    #[test]
    fn i420_round_trips_through_nv12() {
        let (width, height) = (6, 4);
        let i420: Vec<u8> = (0..width * height * 3 / 2)
            .map(|index| u8::try_from(index * 7 % 251).unwrap())
            .collect();
        let frame =
            NativeFrame::from_i420(&YUVBuffer::from_vec(i420.clone(), width, height)).unwrap();
        assert_eq!((frame.width(), frame.height()), (6, 4));
        assert_eq!(frame.to_i420().unwrap(), i420);
    }
}
