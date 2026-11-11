pub(crate) mod analysis_worker;
pub(crate) mod command_runner;
#[cfg(target_os = "linux")]
pub(crate) mod engine_ipc;
#[cfg(target_os = "linux")]
pub(crate) mod engine_server;
pub(crate) mod file_link;
pub(crate) mod legacy_cleanup;
pub(crate) mod loudness;
#[cfg(test)]
pub(crate) mod loudness_acceptance;
pub(crate) mod metadata;
pub(crate) mod pipewire_detection;
pub(crate) mod playback_engine;
#[cfg(target_os = "linux")]
pub(crate) mod player;
#[cfg(windows)]
pub(crate) mod player_win;
pub(crate) mod scanner;

pub use loudness::LoudnessError;
pub use playback_engine::PlaybackEngine;

#[cfg(target_os = "linux")]
pub use player::{
    AudioBackendKind, AudioPlayer, AudioSourceInfo, EngineError, PlaybackPosition, PlayerSnapshot,
};
#[cfg(windows)]
pub use player_win::{
    AudioBackendKind, AudioPlayer, AudioSourceInfo, EngineError, PlaybackPosition, PlayerSnapshot,
};

/// Sample rate used by the shared decoder.
#[allow(dead_code)]
pub(crate) const TARGET_OUTPUT_SAMPLE_RATE: u32 = 48_000;
