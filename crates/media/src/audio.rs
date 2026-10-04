//! System audio: capture on the host, Opus coding, and playback on the viewer.
//!
//! The host captures what the system is playing ([`SystemAudioCapturer`], a loopback stream)
//! and [`spawn_audio_stream`] turns it into 20 ms Opus packets at 48 kHz stereo on its own
//! thread. The viewer hands packets to an [`AudioPlayer`], whose thread decodes them (concealing
//! lost ones) into a [`PlaybackBuffer`] that the output device drains.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc as std_mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Sample rate of every Opus packet.
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
/// Channels of every Opus packet (stereo, interleaved).
pub const AUDIO_CHANNELS: usize = 2;
/// Samples per channel in one packet: 20 ms.
pub const AUDIO_FRAME_SAMPLES: usize = 960;
/// Largest Opus packet (RFC 6716).
pub const MAX_AUDIO_PACKET_BYTES: usize = 1276;
/// Encoder bitrate: transparent for desktop sound, tiny next to video.
const AUDIO_BITRATE_BPS: i32 = 96_000;
/// How long the capture thread waits for samples before checking whether to stop.
const CAPTURE_POLL: Duration = Duration::from_millis(100);
/// Captured chunks buffered between the device callback and the encoder.
const CAPTURE_QUEUE: usize = 64;
/// Packets buffered between the network and the decoder.
const PLAYBACK_QUEUE: usize = 64;
/// Audio buffered before playback starts (and after it ran dry), to absorb network jitter.
const PLAYBACK_PRIME: Duration = Duration::from_millis(60);
/// Most audio buffered; beyond it the oldest is dropped so latency can't creep up.
const PLAYBACK_MAX: Duration = Duration::from_millis(200);
/// Lost packets concealed in a row before the gap is treated as silence.
const MAX_CONCEALED: u32 = 3;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("capturing system audio needs macOS 14.6 or later")]
    UnsupportedOs,
    #[error("this platform cannot capture system audio")]
    Unsupported,
    #[error("no audio device is available")]
    NoDevice,
    #[error("audio device error: {0}")]
    Device(String),
    #[error("audio codec error: {0}")]
    Codec(&'static str),
}

/// Interleaved samples at a device's own rate and channel count.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioChunk {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

/// A source of the host's system audio. Opened and read on the audio thread.
pub trait AudioCapturer {
    /// Waits up to `timeout` for the next samples; `Ok(None)` if none arrived.
    fn next_chunk(&mut self, timeout: Duration) -> Result<Option<AudioChunk>, AudioError>;
}

/// What the system plays, captured with a loopback stream on the default output device
/// (WASAPI loopback on Windows, a Core Audio process tap on macOS 14.6 and later).
pub struct SystemAudioCapturer {
    _stream: cpal::Stream,
    chunks: std_mpsc::Receiver<AudioChunk>,
    failure: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for SystemAudioCapturer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemAudioCapturer")
            .finish_non_exhaustive()
    }
}

impl SystemAudioCapturer {
    pub fn open() -> Result<Self, AudioError> {
        if !loopback_supported() {
            return Err(AudioError::UnsupportedOs);
        }
        let device = cpal::default_host()
            .default_output_device()
            .ok_or(AudioError::NoDevice)?;
        let supported = device
            .default_output_config()
            .map_err(|error| AudioError::Device(error.to_string()))?;
        let sample_format = supported.sample_format();
        let config = supported.config();
        let (sample_rate, channels) = (config.sample_rate, config.channels);
        let (sender, chunks) = std_mpsc::sync_channel(CAPTURE_QUEUE);
        let failure = Arc::new(Mutex::new(None));
        let reported = failure.clone();
        // Building an input stream on an output device records what it plays.
        let stream = device
            .build_input_stream_raw(
                config,
                sample_format,
                move |data: &cpal::Data, _: &cpal::InputCallbackInfo| {
                    let Some(samples) = samples_as_f32(data) else {
                        return;
                    };
                    // Dropping beats blocking the device's real-time thread.
                    let _sent = sender.try_send(AudioChunk {
                        samples,
                        sample_rate,
                        channels,
                    });
                },
                move |error: cpal::Error| {
                    if capture_error_is_fatal(error.kind()) {
                        warn!(%error, "system audio capture failed");
                        *reported.lock().unwrap_or_else(PoisonError::into_inner) =
                            Some(error.to_string());
                    } else {
                        debug!(%error, "system audio capture glitched");
                    }
                },
                None,
            )
            .map_err(|error| AudioError::Device(error.to_string()))?;
        stream
            .play()
            .map_err(|error| AudioError::Device(error.to_string()))?;
        Ok(Self {
            _stream: stream,
            chunks,
            failure,
        })
    }
}

impl AudioCapturer for SystemAudioCapturer {
    fn next_chunk(&mut self, timeout: Duration) -> Result<Option<AudioChunk>, AudioError> {
        if let Some(failure) = self
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return Err(AudioError::Device(failure));
        }
        match self.chunks.recv_timeout(timeout) {
            Ok(chunk) => Ok(Some(chunk)),
            Err(std_mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                Err(AudioError::Device("the capture stream stopped".into()))
            }
        }
    }
}

/// Whether a capture stream error ends capture. Glitches (an underrun or overrun, which a busy
/// or emulated machine reports routinely), rerouting to a new default device, and a refused
/// real-time priority leave the stream running.
fn capture_error_is_fatal(kind: cpal::ErrorKind) -> bool {
    !matches!(
        kind,
        cpal::ErrorKind::Xrun | cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied
    )
}

/// Whether this OS can record its own output. cpal's macOS loopback uses a process tap, which
/// exists from macOS 14.6; the app weak-links Core Audio so it still starts on older systems,
/// where this must keep the tap from ever being called.
fn loopback_supported() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_version().is_some_and(|version| version >= (14, 6))
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// The running macOS version, from the system's version file.
#[cfg(target_os = "macos")]
fn macos_version() -> Option<(u32, u32)> {
    let plist = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
    parse_product_version(&plist)
}

#[cfg(any(target_os = "macos", test))]
fn parse_product_version(plist: &str) -> Option<(u32, u32)> {
    let after_key = plist.split("<key>ProductVersion</key>").nth(1)?;
    let value = after_key
        .split("<string>")
        .nth(1)?
        .split("</string>")
        .next()?;
    let mut parts = value.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |minor| minor.parse().ok())?;
    Some((major, minor))
}

fn samples_as_f32(data: &cpal::Data) -> Option<Vec<f32>> {
    match data.sample_format() {
        cpal::SampleFormat::F32 => data.as_slice::<f32>().map(<[f32]>::to_vec),
        cpal::SampleFormat::I16 => data.as_slice::<i16>().map(|samples| {
            samples
                .iter()
                .map(|sample| f32::from(*sample) / 32_768.)
                .collect()
        }),
        #[expect(clippy::cast_precision_loss, reason = "audio samples")]
        cpal::SampleFormat::I32 => data.as_slice::<i32>().map(|samples| {
            samples
                .iter()
                .map(|sample| *sample as f32 / 2_147_483_648.)
                .collect()
        }),
        other => {
            debug!(?other, "unsupported capture sample format");
            None
        }
    }
}

/// A sine tone, paced in real time, for tests without an audio device.
#[derive(Debug)]
pub struct SyntheticAudioCapturer {
    phase: f32,
    frequency: f32,
}

impl SyntheticAudioCapturer {
    pub fn new(frequency: f32) -> Self {
        Self {
            phase: 0.,
            frequency,
        }
    }
}

impl AudioCapturer for SyntheticAudioCapturer {
    fn next_chunk(&mut self, _timeout: Duration) -> Result<Option<AudioChunk>, AudioError> {
        // 10 ms of 44.1 kHz mono, so tests also exercise resampling and channel mapping.
        const RATE: u32 = 44_100;
        const FRAMES: usize = 441;
        std::thread::sleep(Duration::from_millis(10));
        #[expect(clippy::cast_precision_loss, reason = "a test tone")]
        let step = std::f32::consts::TAU * self.frequency / RATE as f32;
        let samples = (0..FRAMES)
            .map(|_| {
                self.phase = (self.phase + step) % std::f32::consts::TAU;
                self.phase.sin() * 0.5
            })
            .collect();
        Ok(Some(AudioChunk {
            samples,
            sample_rate: RATE,
            channels: 1,
        }))
    }
}

/// Linear-interpolating sample rate converter for interleaved audio, continuous across
/// chunks. Devices almost always run at 48 kHz already; this covers the rest.
#[derive(Debug)]
struct Resampler {
    channels: usize,
    /// Input frames per output frame.
    step: f64,
    /// Position of the next output frame; 0 is `previous` when there is one.
    position: f64,
    previous: Option<Vec<f32>>,
}

impl Resampler {
    fn new(from: u32, to: u32, channels: usize) -> Self {
        Self {
            channels,
            step: f64::from(from) / f64::from(to),
            position: 0.,
            previous: None,
        }
    }

    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let channels = self.channels;
        if (self.step - 1.).abs() < f64::EPSILON || input.len() < channels {
            return input.to_vec();
        }
        let offset = usize::from(self.previous.is_some());
        let frames = input.len() / channels + offset;
        let sample = |frame: usize, channel: usize| -> f32 {
            match (&self.previous, frame) {
                (Some(previous), 0) => previous[channel],
                _ => input[(frame - offset) * channels + channel],
            }
        };
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "frame positions are small and non-negative"
        )]
        let output = {
            let mut output = Vec::with_capacity((frames as f64 / self.step) as usize * channels);
            while self.position + 1. < frames as f64 {
                let frame = self.position.floor() as usize;
                let fraction = (self.position - frame as f64) as f32;
                for channel in 0..channels {
                    let (a, b) = (sample(frame, channel), sample(frame + 1, channel));
                    output.push(a + (b - a) * fraction);
                }
                self.position += self.step;
            }
            self.position -= (frames - 1) as f64;
            output
        };
        self.previous = Some(input[input.len() - channels..].to_vec());
        output
    }
}

/// Maps interleaved `from`-channel audio onto `to` channels: mono is duplicated, extra
/// channels beyond stereo are dropped, and stereo folded to mono is averaged.
fn map_channels(input: &[f32], from: usize, to: usize) -> Vec<f32> {
    if from == to || from == 0 {
        return input.to_vec();
    }
    let mut output = Vec::with_capacity(input.len() / from * to);
    for frame in input.chunks_exact(from) {
        if to == 1 {
            #[expect(clippy::cast_precision_loss, reason = "channel counts are tiny")]
            output.push(frame.iter().sum::<f32>() / from as f32);
        } else {
            for channel in 0..to {
                output.push(match (from, channel) {
                    (1, _) if channel < 2 => frame[0],
                    (_, channel) if channel < from && channel < 2 => frame[channel],
                    _ => 0.,
                });
            }
        }
    }
    output
}

/// Turns device audio into 20 ms Opus packets at 48 kHz stereo.
pub struct AudioEncoder {
    encoder: opus_rs::OpusEncoder,
    resampler: Option<(u32, usize, Resampler)>,
    pending: Vec<f32>,
}

impl std::fmt::Debug for AudioEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioEncoder").finish_non_exhaustive()
    }
}

impl AudioEncoder {
    pub fn new() -> Result<Self, AudioError> {
        let mut encoder =
            opus_rs::OpusEncoder::new(48_000, AUDIO_CHANNELS, opus_rs::Application::Audio)
                .map_err(AudioError::Codec)?;
        encoder.bitrate_bps = AUDIO_BITRATE_BPS;
        Ok(Self {
            encoder,
            resampler: None,
            pending: Vec::new(),
        })
    }

    /// Adds captured audio; returns every packet that is now complete.
    pub fn push(&mut self, chunk: &AudioChunk) -> Result<Vec<Vec<u8>>, AudioError> {
        let channels = usize::from(chunk.channels);
        let stereo = map_channels(&chunk.samples, channels, AUDIO_CHANNELS);
        let format = (chunk.sample_rate, channels);
        if self
            .resampler
            .as_ref()
            .is_none_or(|(rate, channels, _)| (*rate, *channels) != format)
        {
            self.resampler = Some((
                chunk.sample_rate,
                channels,
                Resampler::new(chunk.sample_rate, AUDIO_SAMPLE_RATE, AUDIO_CHANNELS),
            ));
        }
        if let Some((_, _, resampler)) = &mut self.resampler {
            self.pending.extend(resampler.process(&stereo));
        }
        let frame_len = AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS;
        let mut packets = Vec::new();
        let mut output = [0u8; MAX_AUDIO_PACKET_BYTES];
        while self.pending.len() >= frame_len {
            let frame: Vec<f32> = self.pending.drain(..frame_len).collect();
            let written = self
                .encoder
                .encode(&frame, AUDIO_FRAME_SAMPLES, &mut output)
                .map_err(AudioError::Codec)?;
            packets.push(output[..written].to_vec());
        }
        Ok(packets)
    }
}

/// Decodes packets back to 48 kHz stereo, concealing short gaps.
pub struct AudioDecoder {
    decoder: opus_rs::OpusDecoder,
    next_sequence: Option<u32>,
    /// The first byte of the last good packet; decoding it alone conceals a lost frame.
    last_toc: Option<u8>,
}

impl std::fmt::Debug for AudioDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioDecoder").finish_non_exhaustive()
    }
}

impl AudioDecoder {
    pub fn new() -> Result<Self, AudioError> {
        Ok(Self {
            decoder: opus_rs::OpusDecoder::new(48_000, AUDIO_CHANNELS)
                .map_err(AudioError::Codec)?,
            next_sequence: None,
            last_toc: None,
        })
    }

    /// Decodes packet `sequence`. Late or repeated packets yield nothing; packets after a
    /// short gap first yield concealment for the missing frames.
    pub fn decode(&mut self, sequence: u32, packet: &[u8]) -> Result<Vec<f32>, AudioError> {
        let mut output = Vec::new();
        if let Some(expected) = self.next_sequence {
            let ahead = sequence.wrapping_sub(expected);
            if ahead > u32::MAX / 2 {
                return Ok(output);
            }
            if let Some(toc) = self.last_toc {
                for _ in 0..ahead.min(MAX_CONCEALED) {
                    output.extend(self.decode_frame(&[toc])?);
                }
            }
        }
        output.extend(self.decode_frame(packet)?);
        self.next_sequence = Some(sequence.wrapping_add(1));
        self.last_toc = packet.first().copied();
        Ok(output)
    }

    fn decode_frame(&mut self, packet: &[u8]) -> Result<Vec<f32>, AudioError> {
        let mut pcm = vec![0.; AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS];
        let samples = self
            .decoder
            .decode(packet, AUDIO_FRAME_SAMPLES, &mut pcm)
            .map_err(AudioError::Codec)?;
        pcm.truncate(samples * AUDIO_CHANNELS);
        Ok(pcm)
    }
}

/// A running audio capture thread. Dropping it stops the thread.
#[derive(Debug)]
pub struct AudioStream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioStream {
    /// Stops capturing and waits for the thread to exit.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            warn!("audio thread panicked");
        }
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Opens a capturer with `open` on a new thread and sends its audio as Opus packets to
/// `packets`, dropping packets when the consumer falls behind. Returns once the capturer is
/// open, or with the reason it could not be opened.
pub fn spawn_audio_stream<F>(
    open: F,
    packets: mpsc::Sender<Vec<u8>>,
) -> Result<AudioStream, AudioError>
where
    F: FnOnce() -> Result<Box<dyn AudioCapturer>, AudioError> + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let (opened, open_result) = std_mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("dari-audio-capture".into())
        .spawn(move || {
            let setup = open().and_then(|capturer| Ok((capturer, AudioEncoder::new()?)));
            let (mut capturer, mut encoder) = match setup {
                Ok(setup) => {
                    let _sent = opened.send(Ok(()));
                    setup
                }
                Err(error) => {
                    let _sent = opened.send(Err(error));
                    return;
                }
            };
            while !thread_stop.load(Ordering::Relaxed) {
                let chunk = match capturer.next_chunk(CAPTURE_POLL) {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => continue,
                    Err(error) => {
                        warn!(%error, "audio capture stopped");
                        return;
                    }
                };
                match encoder.push(&chunk) {
                    Ok(encoded) => {
                        for packet in encoded {
                            if packets.try_send(packet).is_err() && packets.is_closed() {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        warn!(%error, "audio encoding failed");
                        return;
                    }
                }
            }
        })
        .map_err(|error| AudioError::Device(error.to_string()))?;
    match open_result.recv() {
        Ok(Ok(())) => Ok(AudioStream {
            stop,
            thread: Some(thread),
        }),
        Ok(Err(error)) => {
            let _joined = thread.join();
            Err(error)
        }
        Err(_) => Err(AudioError::Device("the audio thread exited".into())),
    }
}

/// Samples waiting for the output device, at the device's rate and channel count.
///
/// The device drains it with [`PlaybackBuffer::fill`]. Playback starts only once enough is
/// buffered to ride out network jitter, and starts over that way after running dry.
#[derive(Debug, Clone)]
pub struct PlaybackBuffer(Arc<Mutex<PlaybackState>>);

#[derive(Debug)]
struct PlaybackState {
    samples: VecDeque<f32>,
    playing: bool,
    prime: usize,
    max: usize,
}

impl PlaybackBuffer {
    /// An empty buffer for an output running at `sample_rate` with `channels`.
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let per_second = sample_rate as usize * usize::from(channels);
        let samples_for = |duration: Duration| -> usize {
            usize::try_from(duration.as_millis()).unwrap_or(usize::MAX) * per_second / 1000
        };
        Self(Arc::new(Mutex::new(PlaybackState {
            samples: VecDeque::new(),
            playing: false,
            prime: samples_for(PLAYBACK_PRIME),
            max: samples_for(PLAYBACK_MAX),
        })))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, PlaybackState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, samples: &[f32]) {
        let mut state = self.state();
        state.samples.extend(samples);
        let excess = state.samples.len().saturating_sub(state.max);
        state.samples.drain(..excess);
    }

    /// Fills `output` with the next samples, or silence while buffering.
    pub fn fill(&self, output: &mut [f32]) {
        let mut state = self.state();
        if !state.playing && state.samples.len() >= state.prime {
            state.playing = true;
        }
        let mut written = 0;
        if state.playing {
            for (slot, sample) in output.iter_mut().zip(state.samples.drain(..)) {
                *slot = sample;
                written += 1;
            }
            if written < output.len() {
                // Ran dry: wait for a full cushion again rather than stuttering.
                state.playing = false;
            }
        }
        output[written..].fill(0.);
    }

    /// Samples buffered right now.
    pub fn buffered(&self) -> usize {
        self.state().samples.len()
    }
}

/// An open output device draining its [`PlaybackBuffer`]. Dropping it stops playback.
pub trait AudioOutput {
    /// The buffer the device plays from, at the device's rate and channel count.
    fn buffer(&self) -> &PlaybackBuffer;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
}

/// Opens the output that plays a viewer's audio. Called on the playback thread.
pub type AudioOutputFactory =
    Arc<dyn Fn() -> Result<Box<dyn AudioOutput>, AudioError> + Send + Sync>;

/// The default output device.
pub struct SystemAudioOutput {
    _stream: cpal::Stream,
    buffer: PlaybackBuffer,
    sample_rate: u32,
    channels: u16,
}

impl std::fmt::Debug for SystemAudioOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemAudioOutput")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .finish_non_exhaustive()
    }
}

impl SystemAudioOutput {
    /// A factory for [`AudioOutputFactory`] slots.
    pub fn factory() -> AudioOutputFactory {
        Arc::new(|| Self::open().map(|output| Box::new(output) as Box<dyn AudioOutput>))
    }

    fn open() -> Result<Self, AudioError> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or(AudioError::NoDevice)?;
        let supported = preferred_output_config(&device)?;
        let config = supported.config();
        let (sample_rate, channels) = (config.sample_rate, config.channels);
        let playing = PlaybackBuffer::new(sample_rate, channels);
        let buffer = playing.clone();
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                config,
                move |output: &mut [f32], _: &cpal::OutputCallbackInfo| buffer.fill(output),
                |error| warn!(%error, "audio playback failed"),
                None,
            ),
            cpal::SampleFormat::I16 => {
                let mut scratch = Vec::new();
                device.build_output_stream(
                    config,
                    move |output: &mut [i16], _: &cpal::OutputCallbackInfo| {
                        scratch.resize(output.len(), 0.);
                        buffer.fill(&mut scratch);
                        for (slot, sample) in output.iter_mut().zip(&scratch) {
                            #[expect(clippy::cast_possible_truncation, reason = "clamped")]
                            let value = (sample.clamp(-1., 1.) * 32_767.) as i16;
                            *slot = value;
                        }
                    },
                    |error| warn!(%error, "audio playback failed"),
                    None,
                )
            }
            other => return Err(AudioError::Device(format!("unsupported format {other:?}"))),
        }
        .map_err(|error| AudioError::Device(error.to_string()))?;
        stream
            .play()
            .map_err(|error| AudioError::Device(error.to_string()))?;
        Ok(Self {
            _stream: stream,
            buffer: playing,
            sample_rate,
            channels,
        })
    }
}

impl AudioOutput for SystemAudioOutput {
    fn buffer(&self) -> &PlaybackBuffer {
        &self.buffer
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}

/// The device's float or 16-bit stereo-or-more format at 48 kHz if it has one, else its
/// default.
fn preferred_output_config(
    device: &cpal::Device,
) -> Result<cpal::SupportedStreamConfig, AudioError> {
    let preferred = device.supported_output_configs().ok().and_then(|configs| {
        configs
            .filter(|range| {
                range.channels() >= 2
                    && matches!(
                        range.sample_format(),
                        cpal::SampleFormat::F32 | cpal::SampleFormat::I16
                    )
            })
            .find_map(|range| range.try_with_sample_rate(AUDIO_SAMPLE_RATE))
    });
    match preferred {
        Some(config) => Ok(config),
        None => device
            .default_output_config()
            .map_err(|error| AudioError::Device(error.to_string())),
    }
}

/// Plays a viewer's audio: packets go to a decoder thread that feeds the output device.
/// Dropping it stops playback.
#[derive(Debug)]
pub struct AudioPlayer {
    packets: Option<std_mpsc::SyncSender<(u32, Vec<u8>)>>,
    thread: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    /// Starts the playback thread, which opens the output with `open`. If it can't, packets
    /// are silently discarded.
    pub fn start(open: AudioOutputFactory) -> Result<Self, AudioError> {
        let (packets, queue) = std_mpsc::sync_channel::<(u32, Vec<u8>)>(PLAYBACK_QUEUE);
        let thread = std::thread::Builder::new()
            .name("dari-audio-playback".into())
            .spawn(move || {
                let output = match open() {
                    Ok(output) => output,
                    Err(error) => {
                        warn!(%error, "cannot play the host's audio");
                        return;
                    }
                };
                let mut decoder = match AudioDecoder::new() {
                    Ok(decoder) => decoder,
                    Err(error) => {
                        warn!(%error, "cannot start the audio decoder");
                        return;
                    }
                };
                let channels = usize::from(output.channels());
                let mut resampler =
                    Resampler::new(AUDIO_SAMPLE_RATE, output.sample_rate(), AUDIO_CHANNELS);
                while let Ok((sequence, packet)) = queue.recv() {
                    match decoder.decode(sequence, &packet) {
                        Ok(pcm) => {
                            let resampled = resampler.process(&pcm);
                            output.buffer().push(&map_channels(
                                &resampled,
                                AUDIO_CHANNELS,
                                channels,
                            ));
                        }
                        Err(error) => debug!(%error, sequence, "dropping an audio packet"),
                    }
                }
            })
            .map_err(|error| AudioError::Device(error.to_string()))?;
        Ok(Self {
            packets: Some(packets),
            thread: Some(thread),
        })
    }

    /// Queues a packet; dropped if the decoder is behind.
    pub fn push(&self, sequence: u32, packet: Vec<u8>) {
        if let Some(packets) = &self.packets {
            let _queued = packets.try_send((sequence, packet));
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        drop(self.packets.take());
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            warn!("audio playback thread panicked");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(samples: &[f32]) -> f32 {
        #[expect(clippy::cast_precision_loss, reason = "test")]
        let mean = samples.iter().map(|sample| sample * sample).sum::<f32>() / samples.len() as f32;
        mean.sqrt()
    }

    #[test]
    fn glitches_do_not_stop_capture() {
        assert!(!capture_error_is_fatal(cpal::ErrorKind::Xrun));
        assert!(!capture_error_is_fatal(cpal::ErrorKind::DeviceChanged));
        assert!(capture_error_is_fatal(cpal::ErrorKind::DeviceNotAvailable));
        assert!(capture_error_is_fatal(cpal::ErrorKind::StreamInvalidated));
    }

    #[test]
    fn product_versions_parse() {
        let plist = "<dict><key>ProductName</key><string>macOS</string>\
                     <key>ProductVersion</key><string>14.6.1</string></dict>";
        assert_eq!(parse_product_version(plist), Some((14, 6)));
        let plist = "<key>ProductVersion</key>\n\t<string>15</string>";
        assert_eq!(parse_product_version(plist), Some((15, 0)));
        assert_eq!(parse_product_version("<dict></dict>"), None);
    }

    #[test]
    fn resampling_keeps_duration_and_level_across_chunks() {
        let mut resampler = Resampler::new(44_100, 48_000, 2);
        let mut output = Vec::new();
        for _ in 0..10 {
            output.extend(resampler.process(&[0.25; 441 * 2]));
        }
        // 100 ms in, about 100 ms out, without clicks at chunk boundaries.
        let frames = output.len() / 2;
        assert!((4790..=4800).contains(&frames), "{frames} frames");
        assert!(output.iter().all(|sample| (sample - 0.25).abs() < 1e-6));
    }

    #[test]
    fn matching_rates_pass_through() {
        let mut resampler = Resampler::new(48_000, 48_000, 2);
        assert_eq!(
            resampler.process(&[0.1, 0.2, 0.3, 0.4]),
            [0.1, 0.2, 0.3, 0.4]
        );
    }

    #[test]
    fn channels_map_between_layouts() {
        assert_eq!(map_channels(&[0.5, -0.5], 1, 2), [0.5, 0.5, -0.5, -0.5]);
        assert_eq!(map_channels(&[0.2, 0.4], 2, 1), [0.3]);
        assert_eq!(map_channels(&[1., 2., 3., 4., 5., 6.], 6, 2), [1., 2.]);
        assert_eq!(map_channels(&[1., 2.], 2, 4), [1., 2., 0., 0.]);
    }

    #[test]
    fn encoded_audio_decodes_to_the_same_tone() {
        let mut source = SyntheticAudioCapturer::new(440.);
        let mut encoder = AudioEncoder::new().unwrap();
        let mut decoder = AudioDecoder::new().unwrap();
        let mut decoded = Vec::new();
        let mut sequence = 0;
        for _ in 0..30 {
            let chunk = source.next_chunk(Duration::ZERO).unwrap().unwrap();
            for packet in encoder.push(&chunk).unwrap() {
                assert!(packet.len() <= MAX_AUDIO_PACKET_BYTES);
                decoded.extend(decoder.decode(sequence, &packet).unwrap());
                sequence += 1;
            }
        }
        assert!(sequence >= 14, "{sequence} packets from 300 ms");
        // A 0.5-amplitude sine has an RMS of about 0.35; skip the codec's start-up.
        let level = rms(&decoded[decoded.len() / 2..]);
        assert!((0.25..0.45).contains(&level), "rms {level}");
    }

    #[test]
    fn lost_packets_are_concealed_and_late_ones_dropped() {
        let mut source = SyntheticAudioCapturer::new(440.);
        let mut encoder = AudioEncoder::new().unwrap();
        let mut packets = Vec::new();
        while packets.len() < 6 {
            packets.extend(
                encoder
                    .push(&source.next_chunk(Duration::ZERO).unwrap().unwrap())
                    .unwrap(),
            );
        }
        let mut decoder = AudioDecoder::new().unwrap();
        let frame = AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS;
        assert_eq!(decoder.decode(0, &packets[0]).unwrap().len(), frame);
        // Packets 1 and 2 were lost: two concealed frames come before packet 3.
        assert_eq!(decoder.decode(3, &packets[3]).unwrap().len(), 3 * frame);
        // Packet 2 shows up late and is ignored.
        assert_eq!(decoder.decode(2, &packets[2]).unwrap(), Vec::<f32>::new());
        assert_eq!(decoder.decode(4, &packets[4]).unwrap().len(), frame);
    }

    #[test]
    fn playback_waits_for_a_cushion_and_caps_latency() {
        let buffer = PlaybackBuffer::new(1000, 1);
        let mut output = [1.; 10];
        buffer.push(&[0.5; 30]);
        buffer.fill(&mut output);
        assert_eq!(output, [0.; 10], "silence until 60 ms are buffered");
        buffer.push(&[0.5; 40]);
        buffer.fill(&mut output);
        assert_eq!(output, [0.5; 10]);
        buffer.push(&[0.5; 500]);
        assert_eq!(buffer.buffered(), 200, "never more than 200 ms behind");
        let mut drain = [0.; 300];
        buffer.fill(&mut drain);
        assert!(
            drain[..200]
                .iter()
                .all(|sample| (*sample - 0.5).abs() < 1e-6)
        );
        assert!(drain[200..].iter().all(|sample| sample.abs() < 1e-6));
        // After running dry it buffers again before playing.
        buffer.push(&[0.5; 10]);
        buffer.fill(&mut output);
        assert_eq!(output, [0.; 10]);
    }
}
