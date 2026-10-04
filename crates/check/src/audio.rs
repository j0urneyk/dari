//! The audio check: the host plays a tone through its speakers and shares its system audio; the
//! viewer records what it receives and checks that the tone came through.
//!
//! Run `dari-check audio-host` on one machine and `dari-check audio-view` on the other. On
//! macOS the host must run inside an app bundle that declares `NSAudioCaptureUsageDescription`
//! (see `scripts/crosscheck/mac-check-app.sh`); otherwise macOS records silence.

use std::net::Ipv6Addr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
use dari_media::{AudioError, AudioOutput, AudioOutputFactory, PlaybackBuffer, StreamSettings};
use dari_net::DeviceIdentity;
use dari_proto::{Availability, DeviceId};
use dari_session::{
    HostConfig, HostEvent, HostPolicy, SystemPlatform, ViewerConfig, ViewerEvent, ViewerTarget,
    connect_viewer, start_host,
};

use crate::scenario::Verdict;

/// Rate the viewer records at, matching what it decodes so nothing is resampled.
const RECORD_RATE: u32 = 48_000;
/// How long the viewer listens once the host shares audio.
const LISTEN: Duration = Duration::from_secs(6);
/// Loudness of the host's tone, low enough to be bearable in a room.
const TONE_AMPLITUDE: f32 = 0.2;
/// A received tone must be at least this loud (RMS)...
const MIN_LEVEL: f64 = 0.01;
/// ...and this many times stronger at its frequency than at nearby ones.
const MIN_TONE_RATIO: f64 = 20.;

#[derive(Debug, clap::Args)]
pub(crate) struct AudioHostArgs {
    /// UDP port to listen on; 0 picks a free one.
    #[arg(long, default_value_t = 47821)]
    port: u16,
    /// Relay to register with, so the viewer can connect by ID.
    #[arg(long)]
    relay: Option<String>,
    /// Frequency of the tone this machine plays, in hertz.
    #[arg(long, default_value_t = 997.)]
    tone: f32,
    /// Give up if no session has started and ended within this many seconds.
    #[arg(long, default_value_t = 120)]
    timeout: u64,
}

#[derive(Debug, clap::Args)]
pub(crate) struct AudioViewArgs {
    /// The host's address (`IP` or `IP:port`), or with `--relay` its relay ID.
    address: String,
    #[arg(long)]
    relay: Option<String>,
    /// File holding the host's access password; read from stdin when omitted.
    #[arg(long)]
    password_file: Option<PathBuf>,
    /// Frequency of the tone the host plays, in hertz.
    #[arg(long, default_value_t = 997.)]
    tone: f32,
}

/// Plays a sine tone on the default output device until dropped.
struct Tone {
    _stream: cpal::Stream,
}

impl Tone {
    fn play(frequency: f32) -> anyhow::Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| anyhow!("this machine has no audio output device"))?;
        let supported = device.default_output_config()?;
        let config = supported.config();
        let channels = usize::from(config.channels);
        #[expect(clippy::cast_precision_loss, reason = "sample rates are exact in f32")]
        let step = std::f32::consts::TAU * frequency / config.sample_rate as f32;
        let mut phase = 0f32;
        let mut next = move || {
            phase = (phase + step) % std::f32::consts::TAU;
            phase.sin() * TONE_AMPLITUDE
        };
        let stream = match supported.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                config,
                move |output: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    for frame in output.chunks_mut(channels) {
                        frame.fill(next());
                    }
                },
                |error| println!("tone playback failed: {error}"),
                None,
            )?,
            cpal::SampleFormat::I16 => device.build_output_stream(
                config,
                move |output: &mut [i16], _: &cpal::OutputCallbackInfo| {
                    for frame in output.chunks_mut(channels) {
                        #[expect(clippy::cast_possible_truncation, reason = "within ±1")]
                        frame.fill((next() * 32_767.) as i16);
                    }
                },
                |error| println!("tone playback failed: {error}"),
                None,
            )?,
            other => return Err(anyhow!("unsupported output format {other:?}")),
        };
        stream.play()?;
        println!(
            "playing a {frequency} Hz tone at {} Hz, {channels} channels",
            config.sample_rate
        );
        Ok(Self { _stream: stream })
    }
}

/// Shares this machine's system audio while playing a tone, until the viewer leaves.
pub(crate) async fn host(args: AudioHostArgs) -> anyhow::Result<ExitCode> {
    let mut verdict = Verdict::default();
    let _tone = Tone::play(args.tone)?;
    let (handle, mut events) = start_host(
        HostConfig {
            bind_address: (Ipv6Addr::UNSPECIFIED, args.port).into(),
            host_name: crate::device_name(),
            stream: StreamSettings::default(),
            policy: HostPolicy {
                require_approval: false,
                clipboard: false,
                file_transfer: false,
                audio: true,
            },
            downloads: None,
            relay: args.relay.clone(),
        },
        Arc::new(DeviceIdentity::generate().context("cannot create a device identity")?),
        Arc::new(SystemPlatform),
    )
    .context("cannot start hosting")?;
    println!("port: {}", handle.local_address().port());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(args.timeout);
    let (mut started, mut audio, mut ended) = (false, None, None);
    while ended.is_none() {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Err(_) | Ok(None) => break,
            Ok(Some(event)) => match event {
                HostEvent::PasswordChanged(Some(password)) if !started => {
                    println!("password: {}", password.display_text().as_str());
                }
                HostEvent::Relay(dari_session::RelayStatus::Registered(id)) => {
                    println!("relay-id: {}", id.to_string().replace(' ', ""));
                }
                HostEvent::SessionStarted(peer) => {
                    println!("{} connected", peer.name);
                    started = true;
                }
                HostEvent::SessionStatus(status) => {
                    println!("audio: {:?}", status.audio);
                    audio = Some(status.audio);
                }
                HostEvent::SessionEnded { reason, .. } => ended = Some(reason),
                _ => {}
            },
        }
    }
    verdict.check(started, "a viewer connected");
    verdict.check(
        audio == Some(Availability::Available),
        format!("this machine can share its sound (got {audio:?})"),
    );
    verdict.check(ended.is_some(), "the viewer ended the session");
    Ok(verdict.finish())
}

/// Records what the viewer would play instead of playing it.
struct Recorder {
    buffer: PlaybackBuffer,
    stop: Arc<AtomicBool>,
}

impl AudioOutput for Recorder {
    fn buffer(&self) -> &PlaybackBuffer {
        &self.buffer
    }
    fn sample_rate(&self) -> u32 {
        RECORD_RATE
    }
    fn channels(&self) -> u16 {
        1
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn recorder(samples: Arc<Mutex<Vec<f32>>>) -> AudioOutputFactory {
    Arc::new(move || -> Result<Box<dyn AudioOutput>, AudioError> {
        let buffer = PlaybackBuffer::new(RECORD_RATE, 1);
        let stop = Arc::new(AtomicBool::new(false));
        let (playing, stopped, samples) = (buffer.clone(), stop.clone(), samples.clone());
        std::thread::spawn(move || {
            // Drain in real time, as an output device would.
            let mut period = vec![0.; RECORD_RATE as usize / 100];
            while !stopped.load(Ordering::Relaxed) {
                playing.fill(&mut period);
                samples
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .extend_from_slice(&period);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        Ok(Box::new(Recorder { buffer, stop }))
    })
}

/// Connects, asks for the host's sound, and checks the tone arrives.
pub(crate) async fn view(args: AudioViewArgs) -> anyhow::Result<ExitCode> {
    let mut verdict = Verdict::default();
    let password = crate::viewer::read_password(args.password_file.as_ref()).await?;
    let target = match &args.relay {
        Some(relay) => ViewerTarget::Relay {
            relay: relay.clone(),
            id: DeviceId::parse(&args.address).context("with --relay, give the host's relay ID")?,
        },
        None => ViewerTarget::Direct(crate::viewer::parse_address(&args.address)?),
    };
    let samples = Arc::new(Mutex::new(Vec::new()));
    let (viewer, mut events) = connect_viewer(
        ViewerConfig {
            target,
            client_name: crate::device_name(),
            map_shortcut_modifier: false,
            clipboard: None,
            frame_rate: 30,
            downloads: None,
            audio: Some(recorder(samples.clone())),
            play_audio: true,
        },
        &password,
    )
    .await
    .context("cannot connect")?;

    let mut audio = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = events.recv().await {
            match event {
                ViewerEvent::HostStatus(status) => return Some(status.audio),
                ViewerEvent::Ended(reason) => {
                    println!("session ended: {reason}");
                    return None;
                }
                _ => {}
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    if audio == Some(Availability::Available) {
        // The host only learns whether it may record once its capturer opens, and says so in a
        // later status.
        let listened = tokio::time::Instant::now() + LISTEN;
        while let Ok(Some(event)) = tokio::time::timeout_at(listened, events.recv()).await {
            if let ViewerEvent::HostStatus(status) = event {
                audio = Some(status.audio);
            }
        }
        tokio::time::sleep_until(listened).await;
    }
    verdict.check(
        audio == Some(Availability::Available),
        format!("the host offers its sound (got {audio:?})"),
    );
    if audio == Some(Availability::Available) {
        let recorded = std::mem::take(&mut *samples.lock().unwrap_or_else(PoisonError::into_inner));
        // The last half: the jitter buffer has settled and the codec has started.
        let tail = &recorded[recorded.len() / 2..];
        let level = rms(tail);
        let ratio = tone_ratio(tail, args.tone);
        println!(
            "received {:.1} s of audio; rms {level:.4}, tone ratio {ratio:.1}",
            seconds(recorded.len())
        );
        verdict.check(
            level >= MIN_LEVEL,
            format!("the host's sound arrives (rms {level:.4})"),
        );
        verdict.check(
            ratio >= MIN_TONE_RATIO,
            format!(
                "it is the host's {} Hz tone (×{ratio:.1} over nearby frequencies)",
                args.tone
            ),
        );
    }
    viewer.disconnect();
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(verdict.finish())
}

#[expect(clippy::cast_precision_loss, reason = "sample counts")]
fn seconds(samples: usize) -> f64 {
    samples as f64 / f64::from(RECORD_RATE)
}

fn rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.;
    }
    #[expect(clippy::cast_precision_loss, reason = "sample counts")]
    let count = samples.len() as f64;
    (samples
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum::<f64>()
        / count)
        .sqrt()
}

/// Goertzel power of `samples` at `frequency`.
fn power_at(samples: &[f32], frequency: f64) -> f64 {
    let coefficient = 2. * (std::f64::consts::TAU * frequency / f64::from(RECORD_RATE)).cos();
    let (mut previous, mut before) = (0., 0.);
    for sample in samples {
        let current = f64::from(*sample) + coefficient * previous - before;
        before = previous;
        previous = current;
    }
    previous * previous + before * before - coefficient * previous * before
}

/// How much stronger `samples` are at `tone` than at frequencies a third away on either side.
fn tone_ratio(samples: &[f32], tone: f32) -> f64 {
    let tone = f64::from(tone);
    let nearby = f64::midpoint(
        power_at(samples, tone * 0.67),
        power_at(samples, tone * 1.33),
    );
    power_at(samples, tone) / nearby.max(f64::MIN_POSITIVE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frequency: f32, amplitude: f32) -> Vec<f32> {
        #[expect(clippy::cast_precision_loss, reason = "test")]
        (0..RECORD_RATE)
            .map(|index| {
                let t = index as f32 / RECORD_RATE as f32;
                (std::f32::consts::TAU * frequency * t).sin() * amplitude
            })
            .collect()
    }

    #[test]
    fn the_tone_is_told_apart_from_other_sound() {
        let tone = sine(997., 0.2);
        assert!(rms(&tone) > MIN_LEVEL);
        assert!(tone_ratio(&tone, 997.) > MIN_TONE_RATIO);
        // Another tone, or silence, is not it.
        assert!(tone_ratio(&sine(440., 0.2), 997.) < 1.);
        assert!(rms(&vec![0.; 4800]) < MIN_LEVEL);
    }
}
