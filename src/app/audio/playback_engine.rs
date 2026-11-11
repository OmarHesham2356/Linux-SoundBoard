//! Playback transport seam for commands and tests.

#[cfg(target_os = "linux")]
use super::player::{AudioPlayer, EngineError, PlaybackPosition};
#[cfg(windows)]
use super::player_win::{AudioPlayer, EngineError, PlaybackPosition};

/// Playback transport operations invoked by `commands::playback`.
pub trait PlaybackEngine: Send + Sync {
    fn play(
        &self,
        sound_id: &str,
        path: &str,
        base_volume: f32,
        sound_lufs: Option<f64>,
        sound_true_peak_dbtp: Option<f32>,
    ) -> Result<String, EngineError>;

    /// Plays with the trigger options used by the reproduction settings.
    #[allow(clippy::too_many_arguments)]
    fn play_with_options(
        &self,
        sound_id: &str,
        path: &str,
        base_volume: f32,
        sound_lufs: Option<f64>,
        sound_true_peak_dbtp: Option<f32>,
        looping: bool,
        mute_others: bool,
        replace: bool,
    ) -> Result<String, EngineError> {
        let _ = (looping, mute_others, replace);
        self.play(
            sound_id,
            path,
            base_volume,
            sound_lufs,
            sound_true_peak_dbtp,
        )
    }
    fn stop_sound(&self, sound_id: &str) -> Result<(), EngineError>;
    fn stop_all(&self);
    fn seek_playback(&self, play_id: &str, position_ms: u64);
    fn pause(&self, sound_id: &str);
    fn resume(&self, sound_id: &str);
    fn get_playing(&self) -> Vec<String>;
    fn get_playback_positions(&self) -> Vec<PlaybackPosition>;
}

impl PlaybackEngine for AudioPlayer {
    fn play(
        &self,
        sound_id: &str,
        path: &str,
        base_volume: f32,
        sound_lufs: Option<f64>,
        sound_true_peak_dbtp: Option<f32>,
    ) -> Result<String, EngineError> {
        AudioPlayer::play(
            self,
            sound_id,
            path,
            base_volume,
            sound_lufs,
            sound_true_peak_dbtp,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn play_with_options(
        &self,
        sound_id: &str,
        path: &str,
        base_volume: f32,
        sound_lufs: Option<f64>,
        sound_true_peak_dbtp: Option<f32>,
        looping: bool,
        mute_others: bool,
        replace: bool,
    ) -> Result<String, EngineError> {
        AudioPlayer::play_with_options(
            self,
            sound_id,
            path,
            base_volume,
            sound_lufs,
            sound_true_peak_dbtp,
            looping,
            mute_others,
            replace,
        )
    }

    fn stop_sound(&self, sound_id: &str) -> Result<(), EngineError> {
        AudioPlayer::stop_sound(self, sound_id)
    }

    fn stop_all(&self) {
        AudioPlayer::stop_all(self)
    }

    fn seek_playback(&self, play_id: &str, position_ms: u64) {
        AudioPlayer::seek_playback(self, play_id, position_ms)
    }

    fn pause(&self, sound_id: &str) {
        AudioPlayer::pause(self, sound_id)
    }

    fn resume(&self, sound_id: &str) {
        AudioPlayer::resume(self, sound_id)
    }

    fn get_playing(&self) -> Vec<String> {
        AudioPlayer::get_playing(self)
    }

    fn get_playback_positions(&self) -> Vec<PlaybackPosition> {
        AudioPlayer::get_playback_positions(self)
    }
}
