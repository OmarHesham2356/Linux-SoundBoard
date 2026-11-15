//! Windows audio playback.
//!
//! The Linux build routes playback through a persistent PipeWire/PulseAudio
//! engine with a virtual microphone. Windows exposes neither, and an input that
//! other applications can select needs a kernel-mode audio driver that a
//! user-mode application cannot install. This backend therefore decodes sounds
//! with the shared decoder, mixes them into a WASAPI (cpal) output stream, and
//! routes that stream to the output device the user picks. Choosing an
//! installed virtual audio cable's playback device is what makes Discord, OBS,
//! or a game see the soundboard as a microphone.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{debug, info, warn};
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
    samples: Arc<Vec<f32>>,
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
    /// Output device chosen by the user; `None` follows the Windows default.
    requested_device: Option<String>,
    /// Device the running stream is actually writing to.
    active_device: Option<String>,
    /// Cached output list so snapshots never enumerate devices.
    sources: Vec<AudioSourceInfo>,
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
            requested_device: None,
            active_device: None,
            sources: Vec::new(),
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

                let mut left = sample_at(&voice.samples[..], voice.position, 0);
                let mut right = sample_at(&voice.samples[..], voice.position, 1);
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
    cache: Arc<Mutex<SampleCache>>,
    stop: Arc<AtomicBool>,
    reload: Arc<AtomicU64>,
    mic: Arc<Mutex<MicState>>,
    mic_stop: Arc<AtomicBool>,
    mic_thread: Mutex<Option<thread::JoinHandle<()>>>,
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
        match config
            .settings
            .mic_source
            .clone()
            .or_else(default_virtual_cable)
        {
            // A device picked in an earlier session; a missing one is ignored
            // and playback stays on the Windows default output.
            Some(target) => {
                if let Err(error) = player.set_mic_source(Some(target)) {
                    warn!("Ignoring the saved Windows output device: {error}");
                }
            }
            None => info!(
                "No virtual audio cable found; the soundboard plays to the Windows default output only"
            ),
        }
        player.set_mic_volume(config.settings.mic_volume as f32 / 100.0);
        if let Err(error) = player.set_mic_passthrough(config.settings.mic_passthrough) {
            warn!("Could not start microphone passthrough: {error}");
        }
        player
    }

    /// A player that never opens a device or spawns a thread, for tests.
    #[cfg(test)]
    pub(crate) fn new_test_noop() -> Self {
        let mut state = PlayerState::default();
        state.available = true;
        Self {
            state: Arc::new(Mutex::new(state)),
            cache: Arc::new(Mutex::new(SampleCache::default())),
            stop: Arc::new(AtomicBool::new(true)),
            reload: Arc::new(AtomicU64::new(0)),
            mic: Arc::new(Mutex::new(MicState::new())),
            mic_stop: Arc::new(AtomicBool::new(true)),
            mic_thread: Mutex::new(None),
            thread: Mutex::new(None),
        }
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
        let reload = Arc::new(AtomicU64::new(0));
        let mic = Arc::new(Mutex::new(MicState::new()));
        let mic_stop = Arc::new(AtomicBool::new(true));
        state.lock().sources = enumerate_output_sources();
        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let thread_reload = Arc::clone(&reload);
        let thread_mic = Arc::clone(&mic);
        let handle = thread::Builder::new()
            .name("win-audio-output".to_string())
            .spawn(move || run_output_thread(thread_state, thread_stop, thread_reload, thread_mic))
            .ok();

        Self {
            state,
            cache: Arc::new(Mutex::new(SampleCache::default())),
            stop,
            reload,
            mic,
            mic_stop,
            mic_thread: Mutex::new(None),
            thread: Mutex::new(handle),
        }
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        let state = self.state.lock();
        PlayerSnapshot {
            available: state.available,
            playback_positions: state.positions(),
            playing_ids: state.playing_ids(),
            audio_sources: state.sources.clone(),
            active_capture_target: state.active_device.clone(),
        }
    }

    pub fn set_local_volume(&self, volume: f32) {
        self.state.lock().local_volume = volume.clamp(0.0, 1.0);
    }

    pub fn set_mic_volume(&self, volume: f32) {
        self.mic.lock().volume = volume.clamp(0.0, 2.0);
    }

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

    /// Runs or stops microphone capture. The capture is mixed into the virtual
    /// cable output only, so the room does not hear the microphone through the
    /// speakers as well.
    pub fn set_mic_passthrough(&self, enabled: bool) -> Result<(), EngineError> {
        if self.mic.lock().enabled == enabled && enabled == self.mic_thread.lock().is_some() {
            return Ok(());
        }
        self.mic.lock().enabled = enabled;
        if enabled {
            self.start_mic_capture();
        } else {
            self.stop_mic_capture();
        }
        Ok(())
    }

    fn start_mic_capture(&self) {
        let mut thread = self.mic_thread.lock();
        if thread.is_some() {
            return;
        }
        self.mic_stop.store(false, Ordering::Relaxed);
        let shared = Arc::clone(&self.mic);
        let stop = Arc::clone(&self.mic_stop);
        *thread = thread::Builder::new()
            .name("win-mic-capture".to_string())
            .spawn(move || run_mic_thread(shared, stop))
            .ok();
    }

    /// Returns the decoded sound, reusing an earlier decode of the same file so
    /// a hotkey press does not wait for the decoder again.
    fn decode_cached(
        &self,
        path: &str,
    ) -> Result<(Arc<Vec<f32>>, bool, Option<u64>, bool), EngineError> {
        let metadata = std::fs::metadata(path).ok();
        let file_len = metadata.as_ref().map(|meta| meta.len()).unwrap_or(0);
        let modified = metadata.as_ref().and_then(|meta| meta.modified().ok());

        if let Some(entry) = self.cache.lock().entries.get(path) {
            if entry.file_len == file_len && entry.modified == modified {
                return Ok((
                    Arc::clone(&entry.samples),
                    entry.mono,
                    entry.duration_ms,
                    true,
                ));
            }
        }

        let (decoded_samples, mono, duration_ms) = decode_to_samples(path)?;
        if decoded_samples.is_empty() {
            return Err(EngineError::Playback(format!(
                "'{path}' contains no audio samples"
            )));
        }

        let samples = Arc::new(decoded_samples);
        let bytes = samples.len() * std::mem::size_of::<f32>();
        {
            let mut cache = self.cache.lock();
            if cache.bytes + bytes > SAMPLE_CACHE_LIMIT_BYTES {
                // Simplest useful policy: drop the whole cache rather than
                // tracking ages, since a soundboard fits in it comfortably.
                cache.entries.clear();
                cache.bytes = 0;
            }
            cache.bytes += bytes;
            cache.entries.insert(
                path.to_string(),
                CachedSamples {
                    samples: Arc::clone(&samples),
                    mono,
                    duration_ms,
                    file_len,
                    modified,
                },
            );
        }

        Ok((samples, mono, duration_ms, false))
    }

    fn stop_mic_capture(&self) {
        self.mic_stop.store(true, Ordering::Relaxed);
        let handle = self.mic_thread.lock().take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
        self.mic.lock().available = false;
    }

    /// Windows has no virtual microphone driver of its own, so the selected
    /// source is the output device playback is routed to. Pointing it at an
    /// installed virtual cable makes other applications hear the soundboard as
    /// an input device.
    pub fn set_mic_source(&self, source: Option<String>) -> Result<(), EngineError> {
        // "Automatic" means follow a virtual cable when one is installed, the
        // same routing the player picks at startup.
        let source = source.or_else(default_virtual_cable);
        if let Some(name) = source.as_deref() {
            let host = cpal::default_host();
            if !has_output_device(&host, name) {
                return Err(EngineError::Routing(format!(
                    "'{name}' is not an available Windows audio output device"
                )));
            }
        }

        {
            let mut state = self.state.lock();
            state.requested_device = source;
            state.sources = enumerate_output_sources();
        }
        // A counter rather than a flag: a change requested while the output
        // thread is opening a device must not be cleared and forgotten.
        self.reload.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn set_default_source_mode(&self, _mode: DefaultSourceMode) -> Result<(), EngineError> {
        Ok(())
    }

    pub fn set_mic_latency_profile(&self, _profile: MicLatencyProfile) -> Result<(), EngineError> {
        Ok(())
    }

    pub fn list_audio_sources(&self) -> Vec<AudioSourceInfo> {
        let sources = enumerate_output_sources();
        self.state.lock().sources = sources.clone();
        sources
    }

    pub fn active_capture_target(&self) -> Option<String> {
        self.state.lock().active_device.clone()
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
        let started = std::time::Instant::now();
        let (samples, mono, duration_ms, cached) = self.decode_cached(path)?;
        debug!(
            "play '{sound_id}' ready in {:?}{}",
            started.elapsed(),
            if cached { " (decoded earlier)" } else { "" }
        );

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
        self.stop_mic_capture();
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

fn run_output_thread(
    state: Arc<Mutex<PlayerState>>,
    stop: Arc<AtomicBool>,
    reload: Arc<AtomicU64>,
    mic: Arc<Mutex<MicState>>,
) {
    while !stop.load(Ordering::Relaxed) {
        let generation = reload.load(Ordering::Relaxed);
        let requested = state.lock().requested_device.clone();
        state.lock().sources = enumerate_output_sources();
        let host = cpal::default_host();
        if let Err(message) = run_stream(
            &host,
            requested.as_deref(),
            &state,
            &stop,
            &reload,
            generation,
            &mic,
        ) {
            mark_unavailable(&state, &message);
            wait_for_reload(&stop, &reload, generation);
        }
    }
    info!("Windows audio output stopped");
}

/// Opens the requested device and keeps the stream running until a shutdown or
/// a device change is requested.
///
/// When playback targets a virtual cable the mix is also sent to the Windows
/// default output, which mirrors how the Linux build feeds the speakers and the
/// PipeWire virtual microphone at the same time.
fn run_stream(
    host: &cpal::Host,
    requested: Option<&str>,
    state: &Arc<Mutex<PlayerState>>,
    stop: &Arc<AtomicBool>,
    reload: &Arc<AtomicU64>,
    generation: u64,
    mic: &Arc<Mutex<MicState>>,
) -> Result<(), String> {
    let (device, device_name) = resolve_output_device(host, requested)?;
    let mixer = Arc::new(Mutex::new(Mixer::new()));
    // The microphone only belongs on the virtual cable: putting it on a normal
    // output device would play the room back through the speakers.
    let mic_target = is_virtual_device_name(&device_name);
    let (primary, rate) = open_primary(
        &device,
        &device_name,
        state,
        &mixer,
        mic_target.then_some(mic),
    )?;

    if let Err(error) = primary.play() {
        return Err(format!(
            "Could not start playback on '{device_name}': {error}"
        ));
    }

    let monitor = if rate == TARGET_OUTPUT_SAMPLE_RATE && is_virtual_device_name(&device_name) {
        open_monitor_stream(host, &device_name, &mixer, state)
    } else {
        None
    };

    {
        let mut state = state.lock();
        state.available = true;
        state.error = None;
        state.active_device = Some(device_name);
    }

    while !stop.load(Ordering::Relaxed) && reload.load(Ordering::Relaxed) == generation {
        thread::sleep(Duration::from_millis(100));
    }
    drop(monitor);
    drop(primary);
    Ok(())
}

/// One second of 48 kHz stereo audio shared by the soundboard output and the
/// local monitor, so the playlist is mixed once and both devices hear it.
const MIXER_CAPACITY_FRAMES: usize = TARGET_OUTPUT_SAMPLE_RATE as usize;

struct Mixer {
    ring: Vec<f32>,
    written: u64,
    read: u64,
}

impl Mixer {
    fn new() -> Self {
        Self {
            ring: vec![0.0; MIXER_CAPACITY_FRAMES * TARGET_OUTPUT_CHANNELS as usize],
            written: 0,
            read: 0,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        let channels = TARGET_OUTPUT_CHANNELS as usize;
        let frames = samples.len() / channels;
        for frame in 0..frames {
            let slot = (self.written + frame as u64) as usize % MIXER_CAPACITY_FRAMES;
            let target = slot * channels;
            let source = frame * channels;
            self.ring[target] = samples[source];
            self.ring[target + 1] = samples[source + 1];
        }
        self.written += frames as u64;
    }

    /// Copies the next block for the monitor device. A consumer that fell too
    /// far behind drops its backlog instead of replaying stale audio.
    fn pull(&mut self, samples: &mut [f32]) {
        let channels = TARGET_OUTPUT_CHANNELS as usize;
        let frames = samples.len() / channels;
        samples.fill(0.0);

        let backlog = MIXER_CAPACITY_FRAMES as u64 / 2;
        if self.written.saturating_sub(self.read) > backlog {
            self.read = self.written - backlog / 2;
        }
        if self.written.saturating_sub(self.read) < frames as u64 {
            return;
        }

        for frame in 0..frames {
            let slot = (self.read + frame as u64) as usize % MIXER_CAPACITY_FRAMES;
            let source = slot * channels;
            let target = frame * channels;
            samples[target] = self.ring[source];
            samples[target + 1] = self.ring[source + 1];
        }
        self.read += frames as u64;
    }
}

fn open_primary(
    device: &cpal::Device,
    device_name: &str,
    state: &Arc<Mutex<PlayerState>>,
    mixer: &Arc<Mutex<Mixer>>,
    mic: Option<&Arc<Mutex<MicState>>>,
) -> Result<(cpal::Stream, u32), String> {
    let mut attempts: Vec<cpal::SupportedStreamConfig> = Vec::new();
    if let Some(preferred) = choose_48k_config(device) {
        attempts.push(preferred);
    }
    if let Some(default) = choose_output_config(device) {
        let duplicate = attempts.first().is_some_and(|existing| {
            existing.sample_rate() == default.sample_rate()
                && existing.sample_format() == default.sample_format()
                && existing.channels() == default.channels()
        });
        if !duplicate {
            attempts.push(default);
        }
    }
    if attempts.is_empty() {
        return Err(format!("'{device_name}' has no usable output format"));
    }

    let mut last_error = String::new();
    for supported in attempts {
        match open_stream(device, &supported, state, mixer, mic) {
            Ok(stream) => {
                let rate = supported.sample_rate().0;
                info!(
                    "Windows audio output '{}': {} Hz, {} channels, {:?}",
                    device_name,
                    rate,
                    supported.channels(),
                    supported.sample_format()
                );
                return Ok((stream, rate));
            }
            Err(error) => last_error = format!("Could not open '{device_name}': {error}"),
        }
    }
    Err(last_error)
}

fn open_stream(
    device: &cpal::Device,
    supported: &cpal::SupportedStreamConfig,
    state: &Arc<Mutex<PlayerState>>,
    mixer: &Arc<Mutex<Mixer>>,
    mic: Option<&Arc<Mutex<MicState>>>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let config = supported.config();
    let channels = config.channels as usize;
    let rate = config.sample_rate.0;
    let producer_state = Arc::clone(state);
    let producer_mixer = Arc::clone(mixer);
    let producer_mic = mic.map(Arc::clone);
    let error_state = Arc::clone(state);
    let mut reported_first_block = false;
    let mut last_report = std::time::Instant::now();

    build_stereo_stream(
        device,
        &config,
        supported.sample_format(),
        channels,
        rate,
        move |scratch, output_rate| {
            let mut state = producer_state.lock();
            state.render(scratch, TARGET_OUTPUT_CHANNELS as usize, output_rate, 1.0);
            drop(state);
            let soundboard_peak = peak_of(scratch);
            if !reported_first_block {
                reported_first_block = true;
                debug!("Windows output produced its first block (peak {soundboard_peak:.3})");
            }
            // The monitor copies the soundboard mix before the microphone is
            // folded in, so the speakers never play the room back.
            producer_mixer.lock().push(scratch);
            let mic_peak = match &producer_mic {
                Some(mic) => mic.lock().mix_into(scratch),
                None => 0.0,
            };
            if last_report.elapsed() >= std::time::Duration::from_secs(1)
                && (soundboard_peak > 0.001 || mic_peak > 0.001)
            {
                last_report = std::time::Instant::now();
                debug!(
                    "cable mix: soundboard {soundboard_peak:.3}, mic {mic_peak:.3}, output {:.3}",
                    peak_of(scratch)
                );
            }
        },
        move |error| {
            warn!("Windows audio output stream error: {error}");
            error_state.lock().error = Some(error.to_string());
        },
    )
}

/// The device that plays the soundboard back to the user while the cable gets
/// the same mix. Normally that is the Windows default output; when the default
/// output is the cable itself (VB-CABLE likes to take that spot) the first
/// physical output is used instead, otherwise the user would hear nothing.
fn monitor_device(host: &cpal::Host, primary_name: &str) -> Option<cpal::Device> {
    if let Some(device) = host.default_output_device() {
        let name = device.name().ok();
        if name.as_deref() != Some(primary_name) {
            return Some(device);
        }
    }

    let devices = host.output_devices().ok()?;
    devices.into_iter().find(|device| {
        device
            .name()
            .is_ok_and(|name| name != primary_name && !is_virtual_device_name(&name))
    })
}

fn open_monitor_stream(
    host: &cpal::Host,
    primary_name: &str,
    mixer: &Arc<Mutex<Mixer>>,
    state: &Arc<Mutex<PlayerState>>,
) -> Option<cpal::Stream> {
    let device = monitor_device(host, primary_name)?;
    let name = device
        .name()
        .unwrap_or_else(|_| "Windows default output".to_string());

    let supported = choose_48k_config(&device)?;
    let config = supported.config();
    let channels = config.channels as usize;
    let callback_mixer = Arc::clone(mixer);
    let error_state = Arc::clone(state);

    let stream = build_stereo_stream(
        &device,
        &config,
        supported.sample_format(),
        channels,
        TARGET_OUTPUT_SAMPLE_RATE,
        move |scratch, _| callback_mixer.lock().pull(scratch),
        move |error| {
            warn!("Windows monitor output stream error: {error}");
            error_state.lock().error = Some(error.to_string());
        },
    );

    match stream {
        Ok(stream) => match stream.play() {
            Ok(()) => {
                info!("Windows local monitor output: '{name}'");
                Some(stream)
            }
            Err(error) => {
                warn!("Could not start the Windows monitor output '{name}': {error}");
                None
            }
        },
        Err(error) => {
            warn!("Could not open the Windows monitor output '{name}': {error}");
            None
        }
    }
}

fn wait_for_reload(stop: &Arc<AtomicBool>, reload: &Arc<AtomicU64>, generation: u64) {
    while !stop.load(Ordering::Relaxed) && reload.load(Ordering::Relaxed) == generation {
        thread::sleep(Duration::from_millis(100));
    }
}

fn has_output_device(host: &cpal::Host, name: &str) -> bool {
    let Ok(devices) = host.output_devices() else {
        return false;
    };
    devices
        .filter_map(|device| device.name().ok())
        .any(|device_name| device_name == name)
}

fn resolve_output_device(
    host: &cpal::Host,
    requested: Option<&str>,
) -> Result<(cpal::Device, String), String> {
    match requested {
        Some(name) => {
            let devices = host
                .output_devices()
                .map_err(|error| format!("Could not list Windows audio outputs: {error}"))?;
            for device in devices {
                let Ok(device_name) = device.name() else {
                    continue;
                };
                if device_name == name {
                    return Ok((device, device_name));
                }
            }
            Err(format!(
                "'{name}' is no longer an available Windows audio output"
            ))
        }
        None => {
            let device = host
                .default_output_device()
                .ok_or_else(|| "No default Windows audio output device was found".to_string())?;
            let name = device
                .name()
                .unwrap_or_else(|_| "Windows default output".to_string());
            Ok((device, name))
        }
    }
}

/// Virtual cable drivers install themselves as ordinary output devices. Naming
/// them lets the settings list show which device turns the soundboard into a
/// microphone for other applications.
const VIRTUAL_DEVICE_MARKERS: &[&str] = &[
    "cable input",
    "vb-audio",
    "voicemeeter",
    "virtual audio cable",
    "virtual cable",
    "dante virtual soundcard",
    "virtual desktop",
    "scream",
];

fn is_virtual_device_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    VIRTUAL_DEVICE_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

fn enumerate_output_sources() -> Vec<AudioSourceInfo> {
    let host = cpal::default_host();
    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };

    let mut sources = Vec::new();
    // WASAPI reports a render and a loopback endpoint under the same name, so
    // the device list would otherwise show every cable twice.
    let mut seen = std::collections::HashSet::new();
    for device in devices {
        let Ok(name) = device.name() else {
            continue;
        };
        if !seen.insert(name.clone()) {
            continue;
        }
        let is_virtual = is_virtual_device_name(&name);
        debug!(
            "Windows audio output device: '{}' (virtual={is_virtual})",
            name
        );
        sources.push(AudioSourceInfo {
            display_name: if is_virtual {
                format!("{name} (virtual microphone)")
            } else {
                name.clone()
            },
            node_name: name,
            is_virtual,
            is_hardware_backed: !is_virtual,
        });
    }
    sources
}

fn mark_unavailable(state: &Arc<Mutex<PlayerState>>, message: &str) {
    warn!("{message}");
    let mut state = state.lock();
    state.available = false;
    state.error = Some(message.to_string());
    state.active_device = None;
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

/// Preferred format for the shared mix: 48 kHz stereo, which is what WASAPI
/// shared mode uses on virtually every Windows system.
fn choose_48k_config(device: &cpal::Device) -> Option<cpal::SupportedStreamConfig> {
    if let Ok(default) = device.default_output_config() {
        if default.channels() >= 2 && default.sample_rate().0 == TARGET_OUTPUT_SAMPLE_RATE {
            return Some(default);
        }
    }

    let wanted = cpal::SampleRate(TARGET_OUTPUT_SAMPLE_RATE);
    device
        .supported_output_configs()
        .ok()?
        .find(|config| {
            config.channels() >= 2
                && config.min_sample_rate().0 <= TARGET_OUTPUT_SAMPLE_RATE
                && config.max_sample_rate().0 >= TARGET_OUTPUT_SAMPLE_RATE
        })
        .map(|config| config.with_sample_rate(wanted))
}

/// Builds an output stream whose callback produces interleaved stereo f32 and
/// lets the device format write it out.
fn build_stereo_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    channels: usize,
    rate: u32,
    mut produce: impl FnMut(&mut [f32], u32) + Send + 'static,
    error_callback: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut scratch: Vec<f32> = Vec::new();
    match sample_format {
        cpal::SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _| {
                write_stereo(data, channels, rate, &mut scratch, &mut produce);
            },
            error_callback,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_output_stream(
            config,
            move |data: &mut [i16], _| {
                write_stereo(data, channels, rate, &mut scratch, &mut produce);
            },
            error_callback,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_output_stream(
            config,
            move |data: &mut [u16], _| {
                write_stereo(data, channels, rate, &mut scratch, &mut produce);
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
                    write_stereo(data, channels, rate, &mut scratch, &mut produce);
                },
                error_callback,
                None,
            )
        }
    }
}

fn write_stereo<T>(
    data: &mut [T],
    channels: usize,
    rate: u32,
    scratch: &mut Vec<f32>,
    produce: &mut impl FnMut(&mut [f32], u32),
) where
    T: cpal::Sample + cpal::FromSample<f32>,
{
    let channels = channels.max(1);
    let frames = data.len() / channels;
    scratch.clear();
    scratch.resize(frames * TARGET_OUTPUT_CHANNELS as usize, 0.0);
    produce(scratch, rate);

    for frame in 0..frames {
        let base = frame * channels;
        let left = scratch[frame * 2].clamp(-1.0, 1.0);
        let right = scratch[frame * 2 + 1].clamp(-1.0, 1.0);
        for channel in 0..channels {
            let value = match channel {
                0 => left,
                1 => right,
                _ => 0.0,
            };
            data[base + channel] = T::from_sample(value);
        }
    }
}

/// Decoded sounds kept in memory, so pressing a hotkey again starts playback
/// without decoding the file a second time.
const SAMPLE_CACHE_LIMIT_BYTES: usize = 128 * 1024 * 1024;

#[derive(Default)]
struct SampleCache {
    entries: HashMap<String, CachedSamples>,
    bytes: usize,
}

struct CachedSamples {
    samples: Arc<Vec<f32>>,
    mono: bool,
    duration_ms: Option<u64>,
    file_len: u64,
    modified: Option<std::time::SystemTime>,
}

/// Half a second of captured microphone audio.
const MIC_CAPACITY_FRAMES: usize = TARGET_OUTPUT_SAMPLE_RATE as usize / 2;

/// Captured microphone audio waiting to be folded into the virtual cable.
/// Samples arrive at the input device's rate and are resampled on read, so a
/// 44.1 kHz headset microphone still lines up with the 48 kHz mix.
struct MicState {
    samples: Vec<f32>,
    written: u64,
    read: f64,
    rate: u32,
    enabled: bool,
    volume: f32,
    available: bool,
}

impl MicState {
    fn new() -> Self {
        Self {
            samples: vec![0.0; MIC_CAPACITY_FRAMES],
            written: 0,
            read: 0.0,
            rate: TARGET_OUTPUT_SAMPLE_RATE,
            enabled: true,
            volume: 1.0,
            available: false,
        }
    }

    fn push(&mut self, sample: f32, rate: u32) {
        self.rate = rate;
        let slot = self.written as usize % MIC_CAPACITY_FRAMES;
        self.samples[slot] = sample.clamp(-1.0, 1.0);
        self.written += 1;
    }

    /// Adds the microphone to a 48 kHz stereo block in place, returning the peak
    /// of the microphone's own contribution.
    fn mix_into(&mut self, block: &mut [f32]) -> f32 {
        let channels = TARGET_OUTPUT_CHANNELS as usize;
        if !self.enabled || !self.available || self.rate == 0 {
            return 0.0;
        }

        let frames = block.len() / channels;
        let step = self.rate as f64 / TARGET_OUTPUT_SAMPLE_RATE as f64;
        let mut backlog = self.written.saturating_sub(self.read as u64);
        if backlog > MIC_CAPACITY_FRAMES as u64 / 2 {
            // The output fell behind badly; skip ahead instead of replaying speech.
            self.read = self.written as f64;
            backlog = 0;
        }

        // The microphone and the cable run on independent device clocks, so the
        // buffer slowly drifts. Nudging the read rate keeps it near the middle of
        // the ring instead of cutting a hole in the voice every few seconds.
        let correction = if backlog < 2_000 {
            0.998
        } else if backlog > 8_000 {
            1.002
        } else {
            1.0
        };

        // Leave headroom for the voice instead of summing two hot signals into a
        // hard clip, which is what made the mix sound crunchy.
        let headroom = (1.0 - peak_of(block)).clamp(0.35, 1.0);
        let gain = self.volume * headroom;
        let mut mic_peak = 0.0_f32;

        for frame in 0..frames {
            let index = self.read as u64;
            if index + 2 > self.written {
                break;
            }
            let fraction = (self.read - index as f64) as f32;
            let first = self.samples[index as usize % MIC_CAPACITY_FRAMES];
            let second = self.samples[(index as usize + 1) % MIC_CAPACITY_FRAMES];
            let sample = (first + (second - first) * fraction) * gain;
            mic_peak = mic_peak.max(sample.abs());
            let base = frame * channels;
            block[base] += sample;
            block[base + 1] += sample;
            self.read += step * correction;
        }

        for sample in block.iter_mut() {
            *sample = soft_limit(*sample);
        }
        mic_peak
    }
}

/// With no device saved, use an installed virtual cable: it is the only output
/// other applications can listen to, which is the whole point of the soundboard.
fn default_virtual_cable() -> Option<String> {
    let mut cables: Vec<_> = enumerate_output_sources()
        .into_iter()
        .filter(|source| source.is_virtual)
        .collect();
    // VB-CABLE installs a 2-channel and a 16-channel endpoint; other programs
    // listen to the plain "CABLE Output", which belongs to the 2-channel one.
    cables.sort_by_key(|source| cable_rank(&source.node_name));
    cables.into_iter().next().map(|source| {
        info!(
            "Routing to the detected virtual audio cable '{}'",
            source.node_name
        );
        source.node_name
    })
}

fn cable_rank(name: &str) -> u8 {
    let lower = name.to_ascii_lowercase();
    if lower.contains("16ch") {
        2
    } else if lower.contains("cable input") {
        0
    } else {
        1
    }
}

fn peak_of(samples: &[f32]) -> f32 {
    samples
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()))
}

/// Rounds the top off smoothly instead of hard clipping at 1.0, which keeps a
/// loud soundboard and a loud voice from tearing into each other.
fn soft_limit(sample: f32) -> f32 {
    const KNEE: f32 = 0.7;
    let magnitude = sample.abs();
    if magnitude <= KNEE {
        return sample;
    }
    let over = ((magnitude - KNEE) / (1.0 - KNEE)).min(1.0);
    sample.signum() * (KNEE + (1.0 - KNEE) * over.tanh())
}

fn run_mic_thread(shared: Arc<Mutex<MicState>>, stop: Arc<AtomicBool>) {
    let host = cpal::default_host();
    let Some(device) = host.default_input_device() else {
        warn!("No default Windows microphone was found; microphone passthrough is off");
        return;
    };
    let name = device
        .name()
        .unwrap_or_else(|_| "default microphone".to_string());
    let supported = match device.default_input_config() {
        Ok(config) => config,
        Err(error) => {
            warn!("Could not read the format of the microphone '{name}': {error}");
            return;
        }
    };

    let sample_format = supported.sample_format();
    let config = supported.config();
    let channels = config.channels as usize;
    let rate = config.sample_rate.0;
    let callback_state = Arc::clone(&shared);

    let stream = match build_mic_stream(
        &device,
        &config,
        sample_format,
        channels,
        rate,
        callback_state,
    ) {
        Ok(stream) => stream,
        Err(error) => {
            warn!("Could not open the microphone '{name}': {error}");
            return;
        }
    };
    if let Err(error) = stream.play() {
        warn!("Could not start the microphone '{name}': {error}");
        return;
    }

    {
        let mut shared = shared.lock();
        shared.available = true;
        shared.rate = rate;
    }
    info!("Windows microphone passthrough: '{name}' at {rate} Hz, {channels} channels");

    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }
    shared.lock().available = false;
    drop(stream);
}

fn build_mic_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    channels: usize,
    rate: u32,
    shared: Arc<Mutex<MicState>>,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    use cpal::Sample;

    let error_callback = move |error: cpal::StreamError| {
        warn!("Windows microphone stream error: {error}");
    };

    match sample_format {
        cpal::SampleFormat::F32 => device.build_input_stream(
            config,
            move |data: &[f32], _| push_mic_frames(data, channels, rate, &shared, |s| s),
            error_callback,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            config,
            move |data: &[i16], _| push_mic_frames(data, channels, rate, &shared, f32::from_sample),
            error_callback,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            config,
            move |data: &[u16], _| push_mic_frames(data, channels, rate, &shared, f32::from_sample),
            error_callback,
            None,
        ),
        cpal::SampleFormat::I32 => device.build_input_stream(
            config,
            move |data: &[i32], _| push_mic_frames(data, channels, rate, &shared, f32::from_sample),
            error_callback,
            None,
        ),
        other => {
            warn!("Unsupported Windows microphone sample format {other:?}");
            device.build_input_stream(config, move |_: &[f32], _| {}, error_callback, None)
        }
    }
}

fn push_mic_frames<T: Copy>(
    data: &[T],
    channels: usize,
    rate: u32,
    shared: &Arc<Mutex<MicState>>,
    convert: impl Fn(T) -> f32,
) {
    let channels = channels.max(1);
    let frames = data.len() / channels;
    if frames == 0 {
        return;
    }

    let mut shared = shared.lock();
    for frame in 0..frames {
        let base = frame * channels;
        let mut sum = 0.0_f32;
        for channel in 0..channels {
            sum += convert(data[base + channel]);
        }
        shared.push(sum / channels as f32, rate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(frames: usize, value: f32) -> Vec<f32> {
        vec![value; frames * TARGET_OUTPUT_CHANNELS as usize]
    }

    #[test]
    fn microphone_is_folded_into_the_cable_mix() {
        let mut mic = MicState::new();
        mic.available = true;
        for _ in 0..960 {
            mic.push(0.5, TARGET_OUTPUT_SAMPLE_RATE);
        }

        let mut block = vec![0.4_f32; 960];
        let peak = mic.mix_into(&mut block);

        assert!(
            (peak - 0.3).abs() < 0.01,
            "the microphone should leave headroom: {peak}"
        );
        assert!((block[0] - 0.7).abs() < 0.01, "sum was {}", block[0]);
        assert!(block.iter().all(|sample| sample.abs() <= 1.0));
    }

    #[test]
    fn mixer_returns_what_the_soundboard_pushed() {
        let mut mixer = Mixer::new();
        let produced = block(64, 0.25);
        mixer.push(&produced);

        let mut consumed = vec![0.0_f32; produced.len()];
        mixer.pull(&mut consumed);
        assert_eq!(consumed, produced);
    }

    #[test]
    fn mixer_stays_silent_until_the_soundboard_writes() {
        let mut mixer = Mixer::new();
        let mut consumed = vec![1.0_f32; 32 * TARGET_OUTPUT_CHANNELS as usize];
        mixer.pull(&mut consumed);
        assert!(consumed.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn mixer_drops_a_backlog_instead_of_replaying_stale_audio() {
        let mut mixer = Mixer::new();
        mixer.push(&block(MIXER_CAPACITY_FRAMES * 3, 0.5));

        let mut consumed = vec![0.0_f32; 96 * TARGET_OUTPUT_CHANNELS as usize];
        mixer.pull(&mut consumed);

        assert!(consumed.iter().all(|sample| *sample == 0.5));
        assert!(mixer.written - mixer.read <= MIXER_CAPACITY_FRAMES as u64 / 2);
    }

    #[test]
    fn virtual_devices_are_recognised_by_their_device_name() {
        assert!(is_virtual_device_name(
            "CABLE Input (VB-Audio Virtual Cable)"
        ));
        assert!(is_virtual_device_name(
            "VoiceMeeter Input (VB-Audio VoiceMeeter VAIO)"
        ));
        assert!(is_virtual_device_name("Line 1 (Virtual Audio Cable)"));
        assert!(!is_virtual_device_name("Speakers (Realtek(R) Audio)"));
    }
}
