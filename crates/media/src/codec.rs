//! H.264 encoding and decoding.
//!
//! Encoding uses the platform's hardware encoder where there is one (VideoToolbox on macOS, a
//! Media Foundation hardware encoder on Windows) and OpenH264 otherwise. Decoding always uses
//! OpenH264. Every encoder emits the same Annex-B Constrained Baseline stream with BT.601
//! limited-range color, so any viewer decodes any host.

use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
    UsageType,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};
use thiserror::Error;

use crate::frame::CapturedFrame;

/// Largest frame the decoder accepts, matching what any Dari host can produce.
const MAX_DECODED_PIXELS: usize = 3840 * 2160;
#[cfg(any(target_os = "macos", windows, test))]
const ANNEX_B_START_CODE: [u8; 4] = [0, 0, 0, 1];

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("video codec error: {0}")]
    OpenH264(#[from] openh264::Error),
    #[error("frame dimensions {width}x{height} are not supported")]
    UnsupportedDimensions { width: u32, height: u32 },
    #[error("VideoToolbox {operation} failed with status {status}")]
    VideoToolbox {
        operation: &'static str,
        status: i32,
    },
    #[error("{operation} failed with HRESULT {code:#010x}")]
    Windows { operation: &'static str, code: i32 },
    #[error("malformed encoder output: {0}")]
    MalformedOutput(&'static str),
}

/// Encoder tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncoderSettings {
    pub bitrate_bps: u32,
    pub max_fps: f32,
    /// Use the platform's hardware encoder if it has one.
    pub hardware: bool,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self {
            bitrate_bps: 4_000_000,
            max_fps: 30.0,
            hardware: true,
        }
    }
}

/// One encoded access unit.
#[derive(Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub width: u32,
    pub height: u32,
    /// An IDR/I frame that decodes without earlier frames.
    pub keyframe: bool,
    /// H.264 Annex-B bitstream.
    pub data: Vec<u8>,
}

impl std::fmt::Debug for EncodedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodedFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("keyframe", &self.keyframe)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// Encodes captured frames. Frames must have even dimensions (see [`crate::fit_within`]). A
/// change of dimensions re-initializes the encoder, which starts again with a keyframe.
///
/// If the hardware encoder fails, the encoder falls back to OpenH264 for the rest of its life
/// and carries on with a keyframe, so a stream never ends over a hardware hiccup.
#[derive(Debug)]
pub struct VideoEncoder {
    backend: Backend,
}

#[derive(Debug)]
enum Backend {
    OpenH264(Box<SoftwareEncoder>),
    /// VideoToolbox on macOS, Media Foundation on Windows.
    #[cfg(any(target_os = "macos", windows))]
    Hardware(crate::native::HardwareEncoder),
}

impl VideoEncoder {
    /// Creates an encoder. With `settings.hardware`, it uses the platform's hardware encoder if
    /// the machine has one, and OpenH264 otherwise.
    pub fn new(settings: EncoderSettings) -> Result<Self, CodecError> {
        #[cfg(any(target_os = "macos", windows))]
        if settings.hardware
            && let Some(encoder) = crate::native::HardwareEncoder::new(settings)
        {
            return Ok(Self {
                backend: Backend::Hardware(encoder),
            });
        }
        Ok(Self {
            backend: Backend::OpenH264(Box::new(SoftwareEncoder::new(settings)?)),
        })
    }

    /// Whether frames are encoded in hardware.
    pub fn is_hardware(&self) -> bool {
        match self.backend {
            Backend::OpenH264(_) => false,
            #[cfg(any(target_os = "macos", windows))]
            Backend::Hardware(_) => true,
        }
    }

    /// Makes the next encoded frame a keyframe.
    pub fn request_keyframe(&mut self) {
        match &mut self.backend {
            Backend::OpenH264(encoder) => encoder.keyframe_requested = true,
            #[cfg(any(target_os = "macos", windows))]
            Backend::Hardware(encoder) => encoder.request_keyframe(),
        }
    }

    /// Encodes `frame`. Returns `None` when the encoder produced no output for it.
    pub fn encode(&mut self, frame: &CapturedFrame) -> Result<Option<EncodedFrame>, CodecError> {
        let (width, height) = (frame.width(), frame.height());
        if width % 2 != 0 || height % 2 != 0 {
            return Err(CodecError::UnsupportedDimensions { width, height });
        }
        match &mut self.backend {
            Backend::OpenH264(encoder) => encoder.encode(frame),
            #[cfg(any(target_os = "macos", windows))]
            Backend::Hardware(encoder) => match encoder.encode(frame) {
                Ok(encoded) => Ok(encoded),
                Err(error) => {
                    tracing::warn!(%error, "hardware encoding failed; falling back to OpenH264");
                    let mut software = Box::new(SoftwareEncoder::new(encoder.settings())?);
                    let encoded = software.encode(frame);
                    self.backend = Backend::OpenH264(software);
                    encoded
                }
            },
        }
    }
}

/// OpenH264 in its screen-content real-time mode.
struct SoftwareEncoder {
    encoder: Encoder,
    yuv: Option<YUVBuffer>,
    keyframe_requested: bool,
}

impl std::fmt::Debug for SoftwareEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoftwareEncoder").finish_non_exhaustive()
    }
}

impl SoftwareEncoder {
    fn new(settings: EncoderSettings) -> Result<Self, CodecError> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(settings.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(settings.max_fps))
            // Without frame skipping OpenH264 cannot hold the target bitrate. A skipped frame is
            // simply not emitted (`encode` returns `None`), so the reference chain stays valid.
            .skip_frames(true)
            // Neither is supported for screen content; disable them instead of being warned.
            .adaptive_quantization(false)
            .background_detection(false)
            // The transport is reliable; keyframes are only needed on start, resize, or request.
            .intra_frame_period(IntraFramePeriod::from_num_frames(0));
        Ok(Self {
            encoder: Encoder::with_api_config(OpenH264API::from_source(), config)?,
            yuv: None,
            keyframe_requested: false,
        })
    }

    fn encode(&mut self, frame: &CapturedFrame) -> Result<Option<EncodedFrame>, CodecError> {
        let (width, height) = (frame.width(), frame.height());
        let dimensions = (width as usize, height as usize);
        let yuv = match frame {
            CapturedFrame::Rgba(frame) => {
                let yuv = match &mut self.yuv {
                    Some(yuv) if yuv.dimensions() == dimensions => yuv,
                    slot => slot.insert(YUVBuffer::new(dimensions.0, dimensions.1)),
                };
                yuv.read_rgba8(RgbaSliceU8::new(frame.pixels(), dimensions));
                yuv
            }
            #[cfg(any(target_os = "macos", windows))]
            CapturedFrame::Native(frame) => self.yuv.insert(YUVBuffer::from_vec(
                frame.to_i420()?,
                dimensions.0,
                dimensions.1,
            )),
        };
        if std::mem::take(&mut self.keyframe_requested) {
            self.encoder.force_intra_frame();
        }
        let bitstream = self.encoder.encode(&*yuv)?;
        let keyframe = match bitstream.frame_type() {
            FrameType::IDR | FrameType::I => true,
            FrameType::P | FrameType::IPMixed => false,
            FrameType::Skip | FrameType::Invalid => return Ok(None),
        };
        let data = bitstream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedFrame {
            width,
            height,
            keyframe,
            data,
        }))
    }
}

#[cfg(any(target_os = "macos", windows))]
/// Converts RGBA pixels to I420 planes the way OpenH264 does (BT.601, limited range), so both
/// encoders produce the same colors.
pub(crate) fn rgba_to_i420(frame: &crate::frame::RgbaFrame) -> YUVBuffer {
    let dimensions = (frame.width() as usize, frame.height() as usize);
    let mut yuv = YUVBuffer::new(dimensions.0, dimensions.1);
    yuv.read_rgba8(RgbaSliceU8::new(frame.pixels(), dimensions));
    yuv
}

#[cfg(any(target_os = "macos", windows, test))]
/// Rewrites one AVCC access unit (NAL units with big-endian length prefixes, as VideoToolbox
/// emits them) as Annex-B, prefixing `parameter_sets` (SPS and PPS, for a keyframe).
pub(crate) fn avcc_to_annex_b(
    avcc: &[u8],
    length_size: usize,
    parameter_sets: &[&[u8]],
) -> Result<Vec<u8>, CodecError> {
    if !(1..=4).contains(&length_size) {
        return Err(CodecError::MalformedOutput("NAL length size"));
    }
    let mut annex_b = Vec::with_capacity(
        avcc.len()
            + parameter_sets
                .iter()
                .map(|set| set.len() + 4)
                .sum::<usize>()
            + 16,
    );
    for set in parameter_sets {
        annex_b.extend_from_slice(&ANNEX_B_START_CODE);
        annex_b.extend_from_slice(set);
    }
    let mut rest = avcc;
    while !rest.is_empty() {
        let (length, after) = rest
            .split_at_checked(length_size)
            .ok_or(CodecError::MalformedOutput("truncated NAL length"))?;
        let length = length
            .iter()
            .fold(0usize, |total, byte| total << 8 | usize::from(*byte));
        let (unit, after) = after
            .split_at_checked(length)
            .ok_or(CodecError::MalformedOutput("truncated NAL unit"))?;
        if !unit.is_empty() {
            annex_b.extend_from_slice(&ANNEX_B_START_CODE);
            annex_b.extend_from_slice(unit);
        }
        rest = after;
    }
    Ok(annex_b)
}

/// A decoded frame in BGRA byte order, the layout GPUI's image textures expect.
#[derive(Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl std::fmt::Debug for DecodedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// Decodes the H.264 stream sent by a host.
pub struct VideoDecoder {
    decoder: Decoder,
}

impl std::fmt::Debug for VideoDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoDecoder").finish_non_exhaustive()
    }
}

impl VideoDecoder {
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self {
            decoder: Decoder::new()?,
        })
    }

    /// Decodes one access unit. Returns `None` while the decoder needs more data.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>, CodecError> {
        let Some(yuv) = self.decoder.decode(data)? else {
            return Ok(None);
        };
        let (width, height) = yuv.dimensions();
        if width == 0 || height == 0 || width.saturating_mul(height) > MAX_DECODED_PIXELS {
            return Err(CodecError::UnsupportedDimensions {
                width: u32::try_from(width).unwrap_or(u32::MAX),
                height: u32::try_from(height).unwrap_or(u32::MAX),
            });
        }
        let mut bgra = vec![0u8; width * height * 4];
        yuv.write_rgba8(&mut bgra);
        for pixel in bgra.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        Ok(Some(DecodedFrame {
            width: u32::try_from(width).unwrap_or(u32::MAX),
            height: u32::try_from(height).unwrap_or(u32::MAX),
            bgra,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::RgbaFrame;
    use crate::synthetic::SyntheticCapturer;

    /// One encoder per backend this machine has. On Windows, Media Foundation's own software
    /// H.264 encoder also runs through the hardware backend, so its Media Foundation path is
    /// exercised on machines without a hardware encoder (CI runners, VMs) too.
    fn encoders() -> Vec<VideoEncoder> {
        let settings = |hardware| EncoderSettings {
            hardware,
            ..EncoderSettings::default()
        };
        let mut encoders = vec![VideoEncoder::new(settings(false)).unwrap()];
        let hardware = VideoEncoder::new(settings(true)).unwrap();
        assert!(
            hardware.is_hardware() || !cfg!(target_os = "macos"),
            "every Mac has VideoToolbox"
        );
        if hardware.is_hardware() {
            encoders.push(hardware);
        }
        #[cfg(windows)]
        encoders.push(VideoEncoder {
            backend: Backend::Hardware(crate::win::HardwareEncoder::microsoft_software(settings(
                true,
            ))),
        });
        encoders
    }

    fn frame(capturer: &mut SyntheticCapturer) -> CapturedFrame {
        capturer.render().into()
    }

    fn mean_abs_error(rgba: &[u8], bgra: &[u8]) -> f64 {
        let total: u64 = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .zip(bgra.as_chunks::<4>().0)
            .map(|(source, decoded)| {
                u64::from(source[0].abs_diff(decoded[2]))
                    + u64::from(source[1].abs_diff(decoded[1]))
                    + u64::from(source[2].abs_diff(decoded[0]))
            })
            .sum();
        #[expect(clippy::cast_precision_loss)]
        let mean = total as f64 / (rgba.len() / 4 * 3) as f64;
        mean
    }

    #[test]
    fn frames_round_trip_through_the_codec() {
        for mut encoder in encoders() {
            let hardware = encoder.is_hardware();
            let mut capturer = SyntheticCapturer::new(320, 240);
            let mut decoder = VideoDecoder::new().unwrap();
            for index in 0..10 {
                let source = capturer.render();
                let encoded = encoder.encode(&source.clone().into()).unwrap().unwrap();
                assert_eq!(encoder.is_hardware(), hardware, "no fallback to software");
                assert_eq!(
                    encoded.keyframe,
                    index == 0,
                    "only the first frame is a keyframe (hardware: {hardware})"
                );
                let decoded = decoder.decode(&encoded.data).unwrap().unwrap();
                assert_eq!((decoded.width, decoded.height), (320, 240));
                let error = mean_abs_error(source.pixels(), &decoded.bgra);
                assert!(
                    error < 12.0,
                    "frame {index} mean error {error} (hardware: {hardware})"
                );
            }
        }
    }

    #[test]
    fn keyframes_can_be_requested() {
        for mut encoder in encoders() {
            let mut capturer = SyntheticCapturer::new(64, 64);
            encoder.encode(&frame(&mut capturer)).unwrap();
            assert!(
                !encoder
                    .encode(&frame(&mut capturer))
                    .unwrap()
                    .unwrap()
                    .keyframe
            );
            encoder.request_keyframe();
            assert!(
                encoder
                    .encode(&frame(&mut capturer))
                    .unwrap()
                    .unwrap()
                    .keyframe
            );
        }
    }

    #[test]
    fn resolution_change_restarts_with_a_keyframe() {
        for mut encoder in encoders() {
            let mut decoder = VideoDecoder::new().unwrap();
            let mut small = SyntheticCapturer::new(64, 48);
            let mut large = SyntheticCapturer::new(128, 96);
            for _ in 0..2 {
                let encoded = encoder.encode(&frame(&mut small)).unwrap().unwrap();
                decoder.decode(&encoded.data).unwrap();
            }
            let encoded = encoder.encode(&frame(&mut large)).unwrap().unwrap();
            assert!(encoded.keyframe);
            assert_eq!((encoded.width, encoded.height), (128, 96));
            let decoded = decoder.decode(&encoded.data).unwrap().unwrap();
            assert_eq!((decoded.width, decoded.height), (128, 96));
        }
    }

    #[test]
    fn odd_dimensions_are_rejected() {
        for mut encoder in encoders() {
            let frame = RgbaFrame::new(3, 2, vec![0; 24]).unwrap();
            assert!(matches!(
                encoder.encode(&frame.into()),
                Err(CodecError::UnsupportedDimensions { .. })
            ));
        }
    }

    #[test]
    fn avcc_units_become_annex_b_with_parameter_sets_first() {
        let avcc = [0, 0, 0, 2, 0x65, 0xAA, 0, 0, 0, 1, 0x06];
        let annex_b = avcc_to_annex_b(&avcc, 4, &[&[0x67, 1], &[0x68, 2]]).unwrap();
        assert_eq!(
            annex_b,
            [
                0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 0xAA, 0, 0, 0, 1, 0x06
            ]
        );
        let two_byte_lengths = avcc_to_annex_b(&[0, 1, 0x41], 2, &[]).unwrap();
        assert_eq!(two_byte_lengths, [0, 0, 0, 1, 0x41]);
    }

    #[test]
    fn malformed_avcc_is_rejected() {
        assert!(avcc_to_annex_b(&[0, 0, 0, 9, 0x65], 4, &[]).is_err());
        assert!(avcc_to_annex_b(&[0, 0], 4, &[]).is_err());
        assert!(avcc_to_annex_b(&[1, 0x65], 0, &[]).is_err());
    }

    #[test]
    fn garbage_input_does_not_panic() {
        let mut decoder = VideoDecoder::new().unwrap();
        for seed in 0u8..32 {
            let garbage: Vec<u8> = (0..512u16)
                .map(|index| u8::try_from(index % 251).unwrap() ^ seed.wrapping_mul(37))
                .collect();
            let _result = decoder.decode(&garbage);
            let mut with_start_code = vec![0, 0, 0, 1];
            with_start_code.extend_from_slice(&garbage);
            let _result = decoder.decode(&with_start_code);
        }
    }
}
