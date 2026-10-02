//! H.264 encoding and decoding with OpenH264.

use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
    UsageType,
};
use openh264::formats::{RgbaSliceU8, YUVBuffer, YUVSource};
use thiserror::Error;

use crate::frame::RgbaFrame;

/// Largest frame the decoder accepts, matching what any open-desk host can produce.
const MAX_DECODED_PIXELS: usize = 3840 * 2160;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("video codec error: {0}")]
    OpenH264(#[from] openh264::Error),
    #[error("frame dimensions {width}x{height} are not supported")]
    UnsupportedDimensions { width: u32, height: u32 },
}

/// Encoder tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncoderSettings {
    pub bitrate_bps: u32,
    pub max_fps: f32,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self {
            bitrate_bps: 4_000_000,
            max_fps: 30.0,
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

/// Encodes RGBA frames. Frames must have even dimensions (see [`crate::fit_within`]). A change
/// of dimensions re-initializes the encoder, which starts again with a keyframe.
pub struct VideoEncoder {
    encoder: Encoder,
    yuv: Option<YUVBuffer>,
    keyframe_requested: bool,
}

impl std::fmt::Debug for VideoEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoEncoder").finish_non_exhaustive()
    }
}

impl VideoEncoder {
    pub fn new(settings: EncoderSettings) -> Result<Self, CodecError> {
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

    /// Makes the next encoded frame a keyframe.
    pub fn request_keyframe(&mut self) {
        self.keyframe_requested = true;
    }

    /// Encodes `frame`. Returns `None` when the encoder produced no output for it.
    pub fn encode(&mut self, frame: &RgbaFrame) -> Result<Option<EncodedFrame>, CodecError> {
        let (width, height) = (frame.width(), frame.height());
        if width % 2 != 0 || height % 2 != 0 {
            return Err(CodecError::UnsupportedDimensions { width, height });
        }
        let dimensions = (width as usize, height as usize);
        let yuv = match &mut self.yuv {
            Some(yuv) if yuv.dimensions() == dimensions => yuv,
            slot => slot.insert(YUVBuffer::new(dimensions.0, dimensions.1)),
        };
        yuv.read_rgba8(RgbaSliceU8::new(frame.pixels(), dimensions));
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
    use crate::stream::ScreenCapturer;
    use crate::synthetic::SyntheticCapturer;

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
        let mut capturer = SyntheticCapturer::new(320, 240);
        let mut encoder = VideoEncoder::new(EncoderSettings::default()).unwrap();
        let mut decoder = VideoDecoder::new().unwrap();
        for index in 0..10 {
            let frame = capturer.capture().unwrap();
            let encoded = encoder.encode(&frame).unwrap().unwrap();
            assert_eq!(
                encoded.keyframe,
                index == 0,
                "only the first frame is a keyframe"
            );
            let decoded = decoder.decode(&encoded.data).unwrap().unwrap();
            assert_eq!((decoded.width, decoded.height), (320, 240));
            let error = mean_abs_error(frame.pixels(), &decoded.bgra);
            assert!(error < 12.0, "frame {index} mean error {error}");
        }
    }

    #[test]
    fn keyframes_can_be_requested() {
        let mut capturer = SyntheticCapturer::new(64, 64);
        let mut encoder = VideoEncoder::new(EncoderSettings::default()).unwrap();
        encoder.encode(&capturer.capture().unwrap()).unwrap();
        assert!(
            !encoder
                .encode(&capturer.capture().unwrap())
                .unwrap()
                .unwrap()
                .keyframe
        );
        encoder.request_keyframe();
        assert!(
            encoder
                .encode(&capturer.capture().unwrap())
                .unwrap()
                .unwrap()
                .keyframe
        );
    }

    #[test]
    fn resolution_change_restarts_with_a_keyframe() {
        let mut encoder = VideoEncoder::new(EncoderSettings::default()).unwrap();
        let mut decoder = VideoDecoder::new().unwrap();
        let mut small = SyntheticCapturer::new(64, 48);
        let mut large = SyntheticCapturer::new(128, 96);
        for frame in [small.capture(), small.capture()] {
            let encoded = encoder.encode(&frame.unwrap()).unwrap().unwrap();
            decoder.decode(&encoded.data).unwrap();
        }
        let encoded = encoder.encode(&large.capture().unwrap()).unwrap().unwrap();
        assert!(encoded.keyframe);
        assert_eq!((encoded.width, encoded.height), (128, 96));
        let decoded = decoder.decode(&encoded.data).unwrap().unwrap();
        assert_eq!((decoded.width, decoded.height), (128, 96));
    }

    #[test]
    fn odd_dimensions_are_rejected() {
        let mut encoder = VideoEncoder::new(EncoderSettings::default()).unwrap();
        let frame = RgbaFrame::new(3, 2, vec![0; 24]).unwrap();
        assert!(matches!(
            encoder.encode(&frame),
            Err(CodecError::UnsupportedDimensions { .. })
        ));
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
