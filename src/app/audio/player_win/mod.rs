//! Windows audio playback.
//!
//! The Linux build routes playback through a persistent PipeWire/PulseAudio
//! engine with a virtual microphone. Windows has neither, so this backend
//! decodes sounds with the shared decoder and mixes them directly into a
//! WASAPI (cpal) output stream. Virtual-microphone and routing controls are
//! accepted and ignored so the rest of the application keeps one API surface.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{info, warn};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::config::{DefaultSourceMode, MicLatencyProfile};

#[path = "../player/decode.rs"]
mod decode;

pub(crate) use decode::{
    is_strict_audio_container, select_audio_track, AudioSource as DecodedAudioSource,
    PlaybackSource as DecodedPlaybackSource,
};

/// Kept identical to the Linux player error so call sites do not branch.
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum EngineError {
    #[error("{0}")]
    Playback(String),
    #[error("{0}")]
    Routing(String),
    #[error("{0}")]
    Setup(String),
}

pub(crate) const TARGET_OUTPUT_SAMPLE_RATE: u32 = 48_000;
const TARGET_OUTPUT_CHANNELS: u16 = 2;

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct PlaybackPosition {
    pub play_id: String,
    pub sound_id: String,
    pub position_ms: u64,
    pub paused: bool,
    pub finished: bool,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioSourceInfo {
    pub node_name: String,
    pub display_name: String,
    #[serde(default)]
    pub is_virtual: bool,
    #[serde(default)]
    pub is_hardware_backed: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    pub available: bool,
    pub playback_positions: Vec<PlaybackPosition>,
    pub playing_ids: Vec<String>,
    pub audio_sources: Vec<AudioSourceInfo>,
    pub active_capture_target: Option<String>,
}

/// Present for API parity with the Linux backend; only the Windows device is
/// used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioBackendKind {
    PipeWire,
    PulseAudio,
}

struct Voice {
    sound_id: String,
    play_id: String,
    samples: Vec<f32>,
    mono: bool,
    /// Position in 48 kHz frames, fractional to allow device-rate resampling.
    position: f64,
    volume: f32,
    looping: bool,
    paused: bool,
    finished: bool,
    duration_ms: Option<u64>,
}

impl Voice {
    fn frames(&self) -> usize {
        self.samples.len() / TARGET_OUTPUT_CHANNELS as usize
    }

    fn duration_ms(&self) -> Option<u64> {
        self.duration_ms.or_else(|| {
            let frames = self.frames();
            (frames > 0).then(|| (frames as u64 * 1000) / TARGET_OUTPUT_SAMPLE_RATE as u64)
        })
    }

    fn position_ms(&self) -> u64 {
        (self.position.max(0.0) as u64 * 1000) / TARGET_OUTPUT_SAMPLE_RATE as u64
    }
}

struct PlayerState {
    voices: HashMap<String, Voice>,
    order: u64,
    local_volume: f32,
    mute_voice: bool,
    looping: bool,
    solo_play_id: Option<String>,
    available: bool,
    error: Option<String>,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            voices: HashMap::new(),
            order: 0,
            local_volume: 1.0,
            mute_voice: false,
            looping: false,
            solo_play_id: None,
            available: false,
            error: None,
        }
    }
}

impl PlayerState {
    fn fresh_play_id(&self, sound_id: &str) -> String {
        format!("win-{}-{}", sound_id, self.order)
    }

    fn positions(&self) -> Vec<PlaybackPosition> {
        let mut positions: Vec<_> = self
            .voices
            .values()
            .map(|voice| PlaybackPosition {
                play_id: voice.play_id.clone(),
                sound_id: voice.sound_id.clone(),
                position_ms: voice.position_ms(),
                paused: voice.paused,
                finished: voice.finished,
                duration_ms: voice.duration_ms(),
            })
            .collect();
        positions.sort_by(|left, right| {
            left.finished
                .cmp(&right.finished)
                .then_with(|| left.position_ms.cmp(&right.position_ms))
        });
        positions
    }

    fn playing_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for voice in self.voices.values() {
            if !voice.finished && !voice.sound_id.is_empty() && !ids.contains(&voice.sound_id) {
                ids.push(voice.sound_id.clone());
            }
        }
        ids
    }

    fn render(&mut self, output: &mut [f32], channels: usize, rate: u32, volume_scale: f32) {
        output.fill(0.0);
        if channels == 0 {
            return;
        }
        let frames = output.len() / channels;
        let step = TARGET_OUTPUT_SAMPLE_RATE as f64 / rate.max(1) as f64;
        let solo = self.solo_play_id.clone();
        let mute_voice = self.mute_voice;
        let local_volume = self.local_volume;

        for voice in self.voices.values_mut() {
            if voice.paused || voice.finished {
                continue;
            }
            let total_frames = voice.frames();
            if total_frames == 0 {
                voice.finished = true;
                continue;
            }
            let audible = solo.as_deref().is_none_or(|id| id == voice.play_id);
            let volume = (voice.volume * local_volume * volume_scale).clamp(0.0, 1.0);
            let cancel_center = mute_voice && !voice.mono;

            for frame in 0..frames {
                if voice.position >= total_frames as f64 {
                    if voice.looping {
                        voice.position = 0.0;
                    } else {
                        voice.finished = true;
                        break;
                    }
                }

                let mut left = sample_at(&voice.samples, voice.position, 0);
                let mut right = sample_at(&voice.samples, voice.position, 1);
                if cancel_center {
                    let center = (left + right) * 0.5;
                    left -= center;
                    right -= center;
                }

                if audible {
                    let base = frame * channels;
                    output[base] += left * volume;
                    if channels > 1 {
                        output[base + 1] += right * volume;
                    }
                }
                voice.position += step;
            }

            if voice.position >= total_frames as f64 && !voice.looping {
                voice.finished = true;
            }
        }

        // Keep the sum inside the device range without a hard clip per voice.
        for sample in output.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }

        // A finished solo playback must not keep every other sound muted.
        if let Some(solo) = self.solo_play_id.clone() {
            let still_running = self.voices.get(&solo).is_some_and(|voice| !voice.finished);
            if !still_running {
                self.solo_play_id = None;
            }
        }
    }
}

fn sample_at(samples: &[f32], position: f64, channel: usize) -> f32 {
    let index = position as usize;
    let base = index * TARGET_OUTPUT_CHANNELS as usize + channel;
    samples.get(base).copied().unwrap_or(0.0)
}

pub struct AudioPlayer {
    state: Arc<Mutex<PlayerState>>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl AudioPlayer {
    pub fn connect_to_engine() -> Option<Self> {
        None
    }

    pub fn new_with_config(config: &crate::config::Config) -> Self {
        let player = Self::new_inner();
        let volume = if config.settings.local_mute {
            0.0
        } else {
            config.settings.local_volume as f32 / 100.0
        };
        player.set_local_volume(volume);
        player
    }

    pub fn new_with_config_and_audio_backend(
        config: &crate::config::Config,
        _audio_backend: AudioBackendKind,
    ) -> Self {
        Self::new_with_config(config)
    }

    fn new_inner() -> Self {
        let state = Arc::new(Mutex::new(PlayerState::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("win-audio-output".to_string())
            .spawn(move || run_output_thread(thread_state, thread_stop))
            .ok();

        Self {
            state,
            stop,
            thread: Mutex::new(handle),
        }
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        let state = self.state.lock();
        PlayerSnapshot {
            available: state.available,
            playback_positions: state.positions(),
            playing_ids: state.playing_ids(),
            audio_sources: Vec::new(),
            active_capture_target: None,
        }
    }

    pub fn set_local_volume(&self, volume: f32) {
        self.state.lock().local_volume = volume.clamp(0.0, 1.0);
    }

    pub fn set_mic_volume(&self, _volume: f32) {}

    pub fn set_auto_gain_enabled(&self, _enabled: bool) {}

    pub fn set_auto_gain_target(&self, _target_lufs: f64) {}

    pub fn set_auto_gain_mode(&self, _mode: u32) {}

    pub fn set_auto_gain_apply_to(&self, _apply_to: u32) {}

    pub fn set_auto_gain_dynamic_settings(
        &self,
        _window_ms: u32,
        _attack_ms: u32,
        _release_ms: u32,
    ) {
    }

    pub fn set_loudness_boost_enabled(&self, _enabled: bool) {}

    pub fn set_loudness_boost_db(&self, _boost_db: f64) {}

    pub fn set_looping(&self, enabled: bool) {
        let mut state = self.state.lock();
        state.looping = enabled;
        for voice in state.voices.values_mut() {
            voice.looping = enabled;
        }
    }

    pub fn set_mute_voice(&self, enabled: bool) {
        self.state.lock().mute_voice = enabled;
    }

    pub fn set_mic_passthrough(&self, _enabled: bool) -> Result<(), EngineError> {
        Ok(())
    }

    pub fn set_mic_source(&self, _source: Option<String>) -> Result<(), EngineError> {
        Ok(())
    }

    pub fn set_default_source_mode(&self, _mode: DefaultSourceMode) -> Result<(), EngineError> {
        Ok(())
    }

    pub fn set_mic_latency_profile(&self, _profile: MicLatencyProfile) -> Result<(), EngineError> {
        Ok(())
    }

    pub fn list_audio_sources(&self) -> Vec<AudioSourceInfo> {
        Vec::new()
    }

    pub fn active_capture_target(&self) -> Option<String> {
        None
    }

    pub fn play(
        &self,
        sound_id: &str,
        path: &str,
        base_volume: f32,
        _sound_lufs: Option<f64>,
        _sound_true_peak_dbtp: Option<f32>,
    ) -> Result<String, EngineError> {
        self.play_with_options(sound_id, path, base_volume, None, None, false, false, false)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn play_with_options(
        &self,
        sound_id: &str,
        path: &str,
        base_volume: f32,
        _sound_lufs: Option<f64>,
        _sound_true_peak_dbtp: Option<f32>,
        looping: bool,
        mute_others: bool,
        replace: bool,
    ) -> Result<String, EngineError> {
        let (samples, mono, duration_ms) = decode_to_samples(path)?;
        if samples.is_empty() {
            return Err(EngineError::Playback(format!(
                "'{path}' contains no audio samples"
            )));
        }

        let mut state = self.state.lock();
        if !state.available {
            return Err(EngineError::Setup(state.error.clone().unwrap_or_else(
                || "No Windows audio output device is available".to_string(),
            )));
        }
        if replace {
            state.voices.clear();
            state.solo_play_id = None;
        }
        state.order = state.order.wrapping_add(1);
        let play_id = state.fresh_play_id(sound_id);
        let looping = looping || state.looping;
        state.voices.insert(
            play_id.clone(),
            Voice {
                sound_id: sound_id.to_string(),
                play_id: play_id.clone(),
                samples,
                mono,
                position: 0.0,
                volume: base_volume.clamp(0.0, 1.0),
                looping,
                paused: false,
                finished: false,
                duration_ms,
            },
        );
        if mute_others {
            state.solo_play_id = Some(play_id.clone());
        }
        Ok(play_id)
    }

    pub fn stop_sound(&self, sound_id: &str) -> Result<(), EngineError> {
        let mut state = self.state.lock();
        let solo_stopped = state
            .solo_play_id
            .as_ref()
            .and_then(|id| state.voices.get(id))
            .is_some_and(|voice| voice.sound_id == sound_id);
        state.voices.retain(|_, voice| voice.sound_id != sound_id);
        if solo_stopped {
            state.solo_play_id = None;
        }
        Ok(())
    }

    pub fn play_replace(
        &self,
        sound_id: &str,
        path: &str,
        base_volume: f32,
        sound_lufs: Option<f64>,
        sound_true_peak_dbtp: Option<f32>,
    ) -> Result<String, EngineError> {
        self.play_with_options(
            sound_id,
            path,
            base_volume,
            sound_lufs,
            sound_true_peak_dbtp,
            false,
            false,
            true,
        )
    }

    pub fn stop_all(&self) {
        let mut state = self.state.lock();
        state.voices.clear();
        state.solo_play_id = None;
    }

    pub fn seek_playback(&self, play_id: &str, position_ms: u64) {
        let mut state = self.state.lock();
        if let Some(voice) = state.voices.get_mut(play_id) {
            let frame = position_ms as f64 * TARGET_OUTPUT_SAMPLE_RATE as f64 / 1000.0;
            voice.position = frame.min(voice.frames().saturating_sub(1) as f64);
            voice.finished = false;
        }
    }

    pub fn pause(&self, sound_id: &str) {
        let mut state = self.state.lock();
        for voice in state.voices.values_mut() {
            if voice.sound_id == sound_id {
                voice.paused = true;
            }
        }
    }

    pub fn resume(&self, sound_id: &str) {
        let mut state = self.state.lock();
        for voice in state.voices.values_mut() {
            if voice.sound_id == sound_id {
                voice.paused = false;
            }
        }
    }

    pub fn get_playing(&self) -> Vec<String> {
        self.state.lock().playing_ids()
    }

    pub fn is_available(&self) -> bool {
        self.state.lock().available
    }

    pub fn get_playback_positions(&self) -> Vec<PlaybackPosition> {
        self.state.lock().positions()
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.lock().take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

type DecodeResult = (Vec<f32>, bool, Option<u64>);

fn decode_to_samples(path: &str) -> Result<DecodeResult, EngineError> {
    use decode::{AudioSource, ChannelSampleRateConverter, PlaybackSource};

    let source = PlaybackSource::from_path(path)?;
    let mono = source.channels() == 1;
    let duration_ms = source
        .total_duration()
        .map(|duration| duration.as_millis().max(1) as u64);
    let gain = source.output_gain_factor();
    let converter = ChannelSampleRateConverter::new(source, TARGET_OUTPUT_SAMPLE_RATE)
        .ok_or_else(|| EngineError::Playback(format!("'{path}' contains no audio samples")))?;

    let mut samples = Vec::with_capacity(TARGET_OUTPUT_SAMPLE_RATE as usize);
    for sample in converter {
        samples.push((sample as f32 / 32768.0) * gain);
    }
    Ok((samples, mono, duration_ms))
}

fn run_output_thread(state: Arc<Mutex<PlayerState>>, stop: Arc<AtomicBool>) {
    let host = cpal::default_host();
    let device = match host.default_output_device() {
        Some(device) => device,
        None => {
            mark_unavailable(&state, "No default Windows audio output device was found");
            return;
        }
    };

    let config = match choose_output_config(&device) {
        Some(config) => config,
        None => {
            mark_unavailable(
                &state,
                "The default audio device has no usable output format",
            );
            return;
        }
    };

    let sample_format = config.sample_format();
    let stream_config = config.config();
    let channels = stream_config.channels as usize;
    let rate = stream_config.sample_rate.0;
    info!(
        "Windows audio output: {} Hz, {} channels, {:?}",
        rate, channels, sample_format
    );

    let callback_state = Arc::clone(&state);
    let error_state = Arc::clone(&state);
    let result = build_stream(
        &device,
        &stream_config,
        sample_format,
        callback_state,
        channels,
        rate,
        move |error| {
            warn!("Windows audio output stream error: {error}");
            error_state.lock().error = Some(error.to_string());
        },
    );

    let stream = match result {
        Ok(stream) => stream,
        Err(error) => {
            mark_unavailable(&state, &format!("Could not open the audio device: {error}"));
            return;
        }
    };
    if let Err(error) = stream.play() {
        mark_unavailable(&state, &format!("Could not start audio playback: {error}"));
        return;
    }

    {
        let mut state = state.lock();
        state.available = true;
        state.error = None;
    }

    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }
    drop(stream);
    info!("Windows audio output stopped");
}

fn mark_unavailable(state: &Arc<Mutex<PlayerState>>, message: &str) {
    warn!("{message}");
    let mut state = state.lock();
    state.available = false;
    state.error = Some(message.to_string());
}

fn choose_output_config(device: &cpal::Device) -> Option<cpal::SupportedStreamConfig> {
    if let Ok(default) = device.default_output_config() {
        if default.channels() >= 1 {
            return Some(default);
        }
    }
    device
        .supported_output_configs()
        .ok()?
        .find(|config| config.channels() >= 1)
        .map(|config| config.with_max_sample_rate())
}

#[allow(clippy::too_many_arguments)]
fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    state: Arc<Mutex<PlayerState>>,
    channels: usize,
    rate: u32,
    error_callback: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    match sample_format {
        cpal::SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _| {
                let mut state = state.lock();
                state.render(data, channels, rate, 1.0);
            },
            error_callback,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_output_stream(
            config,
            move |data: &mut [i16], _| {
                let mut scratch = vec![0.0_f32; data.len()];
                state.lock().render(&mut scratch, channels, rate, 1.0);
                for (target, sample) in data.iter_mut().zip(scratch) {
                    *target = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                }
            },
            error_callback,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_output_stream(
            config,
            move |data: &mut [u16], _| {
                let mut scratch = vec![0.0_f32; data.len()];
                state.lock().render(&mut scratch, channels, rate, 1.0);
                for (target, sample) in data.iter_mut().zip(scratch) {
                    let value = sample.clamp(-1.0, 1.0) * 0.5 + 0.5;
                    *target = (value * u16::MAX as f32) as u16;
                }
            },
            error_callback,
            None,
        ),
        other => {
            warn!("Unsupported Windows output sample format {other:?}; trying f32");
            let fallback = cpal::StreamConfig {
                channels: config.channels,
                sample_rate: config.sample_rate,
                buffer_size: config.buffer_size,
            };
            device.build_output_stream(
                &fallback,
                move |data: &mut [f32], _| {
                    state.lock().render(data, channels, rate, 1.0);
                },
                error_callback,
                None,
            )
        }
    }
}
