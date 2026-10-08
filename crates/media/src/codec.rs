//! H.264 encoding and decoding.
//!
//! Encoding uses the platform's hardware encoder where there is one (VideoToolbox on macOS, a
//! Media Foundation hardware encoder on Windows) and OpenH264 otherwise. Decoding always uses
//! OpenH264, and the `yuv` crate converts its output to BGRA. Every encoder emits the same
//! Annex-B Constrained Baseline stream with BT.601 limited-range color, so any viewer decodes any
//! host.

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
    #[error("color conversion failed: {0}")]
    ColorConversion(#[from] yuv::YuvError),
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

/// Receives one encoded frame. The encoder calls it once the frame is ready, possibly on another
/// thread, or drops it uncalled if the frame produced no output.
pub type FrameDelivery = Box<dyn FnOnce(EncodedFrame) + Send>;

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
    #[cfg(target_os = "macos")]
    VideoToolbox(crate::apple::HardwareEncoder),
    /// Encodes one frame at a time: each is delivered before `submit` returns.
    #[cfg(windows)]
    MediaFoundation(crate::win::HardwareEncoder),
}

impl VideoEncoder {
    /// Creates an encoder. With `settings.hardware`, it uses the platform's hardware encoder if
    /// the machine has one, and OpenH264 otherwise.
    pub fn new(settings: EncoderSettings) -> Result<Self, CodecError> {
        #[cfg(target_os = "macos")]
        if settings.hardware {
            return Ok(Self {
                backend: Backend::VideoToolbox(crate::apple::HardwareEncoder::new(settings)),
            });
        }
        #[cfg(windows)]
        if settings.hardware
            && let Some(encoder) = crate::win::HardwareEncoder::new(settings)
        {
            return Ok(Self {
                backend: Backend::MediaFoundation(encoder),
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
            #[cfg(target_os = "macos")]
            Backend::VideoToolbox(_) => true,
            #[cfg(windows)]
            Backend::MediaFoundation(_) => true,
        }
    }

    /// Makes the next encoded frame a keyframe.
    pub fn request_keyframe(&mut self) {
        match &mut self.backend {
            Backend::OpenH264(encoder) => encoder.keyframe_requested = true,
            #[cfg(target_os = "macos")]
            Backend::VideoToolbox(encoder) => encoder.request_keyframe(),
            #[cfg(windows)]
            Backend::MediaFoundation(encoder) => encoder.request_keyframe(),
        }
    }

    /// Whether a submitted frame was lost to a hardware failure. The next frame switches to
    /// OpenH264 and starts with a keyframe, so a still screen should submit its last frame again.
    pub fn has_lost_frames(&self) -> bool {
        match &self.backend {
            // Media Foundation frames are re-encoded by OpenH264 as soon as they fail.
            Backend::OpenH264(_) => false,
            #[cfg(target_os = "macos")]
            Backend::VideoToolbox(encoder) => encoder.has_failed(),
            #[cfg(windows)]
            Backend::MediaFoundation(_) => false,
        }
    }

    /// Starts encoding `frame` and returns without waiting for it if the backend can overlap
    /// frames (VideoToolbox); OpenH264 encodes before returning. `deliver` receives the result
    /// as soon as it is ready, frames in submission order.
    pub fn submit(
        &mut self,
        frame: &CapturedFrame,
        deliver: FrameDelivery,
    ) -> Result<(), CodecError> {
        let (width, height) = (frame.width(), frame.height());
        if width % 2 != 0 || height % 2 != 0 {
            return Err(CodecError::UnsupportedDimensions { width, height });
        }
        match &mut self.backend {
            Backend::OpenH264(encoder) => encoder.submit(frame, deliver),
            #[cfg(target_os = "macos")]
            Backend::VideoToolbox(encoder) => {
                let deliver = if encoder.has_failed() {
                    tracing::warn!("hardware encoding failed; falling back to OpenH264");
                    Some(deliver)
                } else {
                    match encoder.submit(frame, deliver) {
                        Ok(()) => return Ok(()),
                        Err(failure) => {
                            let error = failure.error;
                            tracing::warn!(%error, "hardware encoding failed; falling back to OpenH264");
                            failure.deliver
                        }
                    }
                };
                let software = Box::new(SoftwareEncoder::new(encoder.settings())?);
                // Dropping the hardware encoder delivers its frames still in flight first.
                self.backend = Backend::OpenH264(software);
                deliver.map_or(Ok(()), |deliver| self.submit(frame, deliver))
            }
            #[cfg(windows)]
            Backend::MediaFoundation(encoder) => match encoder.encode(frame) {
                Ok(encoded) => {
                    if let Some(encoded) = encoded {
                        deliver(encoded);
                    }
                    Ok(())
                }
                Err(error) => {
                    tracing::warn!(%error, "hardware encoding failed; falling back to OpenH264");
                    let software = Box::new(SoftwareEncoder::new(encoder.settings())?);
                    self.backend = Backend::OpenH264(software);
                    self.submit(frame, deliver)
                }
            },
        }
    }

    /// Waits until every submitted frame has been delivered or dropped.
    pub fn flush(&mut self) {
        match &mut self.backend {
            Backend::OpenH264(_) => {}
            #[cfg(target_os = "macos")]
            Backend::VideoToolbox(encoder) => encoder.flush(),
            #[cfg(windows)]
            Backend::MediaFoundation(_) => {}
        }
    }

    /// Encodes `frame` and waits for it. Returns `None` when the encoder produced no output for
    /// it.
    pub fn encode(&mut self, frame: &CapturedFrame) -> Result<Option<EncodedFrame>, CodecError> {
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        for _attempt in 0..2 {
            let output = slot.clone();
            self.submit(
                frame,
                Box::new(move |encoded| {
                    *output
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(encoded);
                }),
            )?;
            self.flush();
            let encoded = slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            // A frame lost to a hardware failure is encoded again by the fallback.
            if encoded.is_some() || !self.has_lost_frames() {
                return Ok(encoded);
            }
        }
        Ok(None)
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

    fn submit(&mut self, frame: &CapturedFrame, deliver: FrameDelivery) -> Result<(), CodecError> {
        if let Some(encoded) = self.encode(frame)? {
            deliver(encoded);
        }
        Ok(())
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

/// H.264's sequence parameter set NAL unit type.
#[cfg(any(target_os = "macos", windows, test))]
const NAL_SPS: u8 = 7;

#[cfg(any(target_os = "macos", windows, test))]
/// The highest H.264 level OpenH264 decodes (`level_idc` 52, level 5.2).
const MAX_DECODABLE_LEVEL: u8 = 52;

#[cfg(any(target_os = "macos", windows, test))]
/// Lowers the level an SPS declares to [`MAX_DECODABLE_LEVEL`]. The automatic level follows the
/// macroblock rate, so 2560×1662 at 144 fps comes out as level 6.0, whose parameter sets OpenH264
/// rejects. The level only states a throughput the decoder must sustain; frame size and
/// reference frames, which it does use, are within 5.2's limits for any stream Dari sends.
pub(crate) fn cap_level(nal: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    // NAL header, profile_idc, constraint flags, level_idc: fixed bytes before any emulation
    // prevention could shift them, since profile_idc is never zero.
    match nal {
        [header, _profile, _constraints, level, ..]
            if header & 0x1f == NAL_SPS && *level > MAX_DECODABLE_LEVEL =>
        {
            let mut capped = nal.to_vec();
            capped[3] = MAX_DECODABLE_LEVEL;
            std::borrow::Cow::Owned(capped)
        }
        _ => std::borrow::Cow::Borrowed(nal),
    }
}

/// Applies [`cap_level`] to every SPS in an Annex-B stream, in place.
#[cfg(any(windows, test))]
pub(crate) fn cap_annex_b_levels(annex_b: &mut [u8]) {
    let starts: Vec<usize> = annex_b
        .windows(3)
        .enumerate()
        .filter(|(_, window)| *window == [0, 0, 1])
        .map(|(index, _)| index + 3)
        .collect();
    for start in starts {
        if let Some(nal) = annex_b.get_mut(start..start + 4)
            && let std::borrow::Cow::Owned(capped) = cap_level(nal)
        {
            nal.copy_from_slice(&capped);
        }
    }
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
        let unsupported = || CodecError::UnsupportedDimensions {
            width: u32::try_from(width).unwrap_or(u32::MAX),
            height: u32::try_from(height).unwrap_or(u32::MAX),
        };
        if width == 0 || height == 0 || width.saturating_mul(height) > MAX_DECODED_PIXELS {
            return Err(unsupported());
        }
        let (y_stride, u_stride, v_stride) = yuv.strides();
        let to_u32 = |value: usize| u32::try_from(value).map_err(|_| unsupported());
        let image = yuv::YuvPlanarImage {
            y_plane: yuv.y(),
            y_stride: to_u32(y_stride)?,
            u_plane: yuv.u(),
            u_stride: to_u32(u_stride)?,
            v_plane: yuv.v(),
            v_stride: to_u32(v_stride)?,
            width: to_u32(width)?,
            height: to_u32(height)?,
        };
        // One SIMD pass straight to BGRA, with the encoders' BT.601 limited-range matrix. It
        // replaced OpenH264's `write_rgba8` and an R/B swap, which took about four times as long
        // as decoding itself.
        let mut bgra = vec![0u8; width * height * 4];
        yuv::yuv420_to_bgra(
            &image,
            &mut bgra,
            image.width * 4,
            yuv::YuvRange::Limited,
            yuv::YuvStandardMatrix::Bt601,
        )?;
        Ok(Some(DecodedFrame {
            width: image.width,
            height: image.height,
            bgra,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::RgbaFrame;
    use crate::synthetic::SyntheticCapturer;

    /// One encoder per backend this platform has.
    fn encoders() -> Vec<VideoEncoder> {
        encoders_with(EncoderSettings::default())
    }

    /// One encoder per backend this machine has. On Windows, Media Foundation's own software
    /// H.264 encoder also runs through the hardware backend, so its Media Foundation path is
    /// exercised on machines without a hardware encoder (CI runners, VMs) too.
    fn encoders_with(settings: EncoderSettings) -> Vec<VideoEncoder> {
        let settings = |hardware| EncoderSettings {
            hardware,
            ..settings
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
            backend: Backend::MediaFoundation(crate::win::HardwareEncoder::microsoft_software(
                settings(true),
            )),
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
    fn decoded_colors_keep_bt601_limited_range() {
        // Saturated colors move by tens of levels under the wrong matrix or range.
        let colors = [
            [0, 0, 0],
            [255, 255, 255],
            [128, 128, 128],
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [240, 64, 32],
        ];
        for mut encoder in encoders() {
            let hardware = encoder.is_hardware();
            let mut decoder = VideoDecoder::new().unwrap();
            for [red, green, blue] in colors {
                let pixels = [red, green, blue, 255].repeat(64 * 64);
                let source = RgbaFrame::new(64, 64, pixels.clone()).unwrap();
                encoder.request_keyframe();
                let encoded = encoder.encode(&source.into()).unwrap().unwrap();
                let decoded = decoder.decode(&encoded.data).unwrap().unwrap();
                let error = mean_abs_error(&pixels, &decoded.bgra);
                assert!(
                    error < 3.0,
                    "{:?} decoded as {:?}, mean error {error} (hardware: {hardware})",
                    [red, green, blue],
                    &decoded.bgra[..4]
                );
            }
        }
    }

    /// Measures decoding the Quality preset's size to BGRA, the viewer's per-frame cost:
    /// `cargo test --release -p dari-media -- --ignored --nocapture decoding_keeps_up`.
    #[test]
    #[ignore = "a release-mode throughput measurement"]
    fn decoding_keeps_up_with_144_fps() {
        let mut encoder = VideoEncoder::new(EncoderSettings {
            max_fps: 144.0,
            hardware: false,
            ..EncoderSettings::default()
        })
        .unwrap();
        let mut capturer = SyntheticCapturer::new(2560, 1662);
        let packets: Vec<_> = (0..32)
            .filter_map(|_| encoder.encode(&frame(&mut capturer)).unwrap())
            .collect();
        let mut decoder = VideoDecoder::new().unwrap();
        let started = std::time::Instant::now();
        for packet in &packets {
            decoder.decode(&packet.data).unwrap().unwrap();
        }
        let per_frame = started.elapsed() / u32::try_from(packets.len()).unwrap();
        println!("2560x1662 decoded to BGRA in {per_frame:?} per frame");
        assert!(
            per_frame < std::time::Duration::from_secs(1) / 144,
            "{per_frame:?} per frame is too slow for 144 fps"
        );
    }

    #[test]
    fn keyframes_come_only_at_the_start() {
        // VideoToolbox's default interval put a keyframe every 30 frames: each cost as much as
        // the first and dropped the picture's quality for a second.
        for mut encoder in encoders() {
            let hardware = encoder.is_hardware();
            let mut capturer = SyntheticCapturer::new(320, 240);
            let keyframes: Vec<usize> = (0..70)
                .filter(|_| {
                    encoder
                        .encode(&frame(&mut capturer))
                        .unwrap()
                        .is_some_and(|encoded| encoded.keyframe)
                })
                .collect();
            assert_eq!(keyframes, [0], "keyframes (hardware: {hardware})");
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

    /// A delivery that forwards to a channel.
    fn deliver_to(sender: &std::sync::mpsc::Sender<EncodedFrame>) -> FrameDelivery {
        let sender = sender.clone();
        Box::new(move |encoded| {
            let _sent = sender.send(encoded);
        })
    }

    #[test]
    fn an_isolated_frame_is_delivered_without_a_following_frame() {
        for mut encoder in encoders() {
            let mut capturer = SyntheticCapturer::new(320, 240);
            let (sender, receiver) = std::sync::mpsc::channel();
            for index in 0..3 {
                encoder
                    .submit(&frame(&mut capturer), deliver_to(&sender))
                    .unwrap();
                // Neither a flush nor a next frame: a keystroke on a still screen must not wait.
                let encoded = receiver
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap_or_else(|_| panic!("frame {index} was held back"));
                assert_eq!(encoded.keyframe, index == 0);
            }
        }
    }

    #[test]
    fn overlapping_frames_arrive_in_order() {
        for mut encoder in encoders() {
            let hardware = encoder.is_hardware();
            let mut capturer = SyntheticCapturer::new(320, 240);
            let mut decoder = VideoDecoder::new().unwrap();
            let (sender, receiver) = std::sync::mpsc::channel();
            for _ in 0..20 {
                encoder
                    .submit(&frame(&mut capturer), deliver_to(&sender))
                    .unwrap();
            }
            encoder.flush();
            assert_eq!(encoder.is_hardware(), hardware, "no fallback to software");
            let frames: Vec<_> = receiver.try_iter().collect();
            assert!(
                frames.len() >= 10,
                "only {} frames (hardware: {hardware})",
                frames.len()
            );
            assert!(frames[0].keyframe);
            for encoded in &frames {
                assert!(decoder.decode(&encoded.data).unwrap().is_some());
            }
        }
    }

    #[test]
    fn a_large_fast_stream_stays_decodable() {
        // 2560×1662 at 144 fps is past level 5.2's macroblock rate, the highest OpenH264 knows.
        for mut encoder in encoders_with(EncoderSettings {
            max_fps: 144.0,
            ..EncoderSettings::default()
        }) {
            let hardware = encoder.is_hardware();
            let mut capturer = SyntheticCapturer::new(2560, 1662);
            let mut decoder = VideoDecoder::new().unwrap();
            for _ in 0..2 {
                let encoded = encoder.encode(&frame(&mut capturer)).unwrap().unwrap();
                let decoded = decoder.decode(&encoded.data);
                assert!(
                    matches!(decoded, Ok(Some(_))),
                    "{decoded:?} (hardware: {hardware})"
                );
            }
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
    fn levels_above_5_2_are_capped() {
        use std::borrow::Cow;
        let level_6 = [0x27, 0x42, 0xc0, 0x3c, 0xab, 0x40];
        assert_eq!(*cap_level(&level_6), [0x27, 0x42, 0xc0, 0x34, 0xab, 0x40]);
        let level_5_1 = [0x27, 0x42, 0xc0, 0x33, 0xab, 0x40];
        assert!(matches!(cap_level(&level_5_1), Cow::Borrowed(_)));
        let pps = [0x28, 0xce, 0x3c, 0x80];
        assert!(matches!(cap_level(&pps), Cow::Borrowed(_)));
    }

    #[test]
    fn levels_are_capped_inside_an_annex_b_stream() {
        let mut stream = [
            0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x3c, 0xab, 0, 0, 1, 0x65, 0x3c,
        ];
        cap_annex_b_levels(&mut stream);
        assert_eq!(
            stream,
            [
                0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x34, 0xab, 0, 0, 1, 0x65, 0x3c
            ]
        );
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
