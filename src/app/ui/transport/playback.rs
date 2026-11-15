use std::rc::Rc;
use std::sync::Arc;

use gtk4::prelude::*;
use gtk4::ToggleButton;

use crate::app_state::AppState;
use crate::commands;
use crate::config::{PlayMode, TriggerMode};

use super::helpers::should_clear_continue_suppression;
use super::{ContinueCursor, TransportBar, TransportInner};
use crate::ui::icons;

#[cfg(test)]
mod tests {
    use super::CycleMode;
    use crate::config::{PlayMode, TriggerMode};

    #[test]
    fn the_mode_button_walks_every_play_mode_then_play_and_hold() {
        assert_eq!(CycleMode::Default.next(), CycleMode::Loop);
        assert_eq!(CycleMode::Loop.next(), CycleMode::Continue);
        assert_eq!(CycleMode::Continue.next(), CycleMode::PlayAndHold);
        assert_eq!(CycleMode::PlayAndHold.next(), CycleMode::Default);

        assert_eq!(
            CycleMode::from_settings(PlayMode::Continue, TriggerMode::Stop),
            CycleMode::Continue
        );
        assert_eq!(
            CycleMode::from_settings(PlayMode::Default, TriggerMode::PlayAndHold),
            CycleMode::PlayAndHold
        );
    }
}

fn set_active_if_changed(button: &ToggleButton, active: bool) {
    if button.is_active() != active {
        button.set_active(active);
    }
}

fn set_active_class<W: IsA<gtk4::Widget>>(widget: &W, active: bool) {
    let widget = widget.as_ref();
    let has_class = widget.has_css_class("btn-active");
    if active && !has_class {
        widget.add_css_class("btn-active");
    } else if !active && has_class {
        widget.remove_css_class("btn-active");
    }
}

fn set_tooltip_if_changed<W: IsA<gtk4::Widget>>(widget: &W, tooltip: &str) {
    let widget = widget.as_ref();
    if widget.tooltip_text().as_deref() != Some(tooltip) {
        widget.set_tooltip_text(Some(tooltip));
    }
}

pub(super) fn update_play_pause_button(button: &ToggleButton, is_playing: bool) {
    set_active_if_changed(button, is_playing);
    set_active_class(button, is_playing);
    if is_playing {
        icons::apply_button_icon(button, icons::PAUSE);
        set_tooltip_if_changed(button, "Pause");
    } else {
        icons::apply_button_icon(button, icons::PLAY);
        set_tooltip_if_changed(button, "Play");
    }
}

pub(super) fn update_mic_button(button: &ToggleButton, enabled: bool) {
    set_active_class(button, enabled);
    if enabled {
        icons::apply_button_icon(button, icons::MICROPHONE);
    } else {
        icons::apply_button_icon(button, icons::MICROPHONE_DISABLED);
    }
}

pub(super) fn update_headphones_button(button: &ToggleButton, enabled: bool) {
    set_active_class(button, enabled);
    if enabled {
        icons::apply_button_icon(button, icons::HEADPHONES);
    } else {
        icons::apply_button_icon(button, icons::HEADPHONES_MUTED);
    }
}

/// The transport's single mode button walks these in order. The first three are
/// the per-track play modes; the last turns a binding into a press-and-release
/// hold, which is why it also writes the trigger setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CycleMode {
    Default,
    Loop,
    Continue,
    PlayAndHold,
}

impl CycleMode {
    pub(super) fn from_settings(play_mode: PlayMode, trigger_mode: TriggerMode) -> Self {
        if trigger_mode == TriggerMode::PlayAndHold {
            return Self::PlayAndHold;
        }
        match play_mode {
            PlayMode::Default => Self::Default,
            PlayMode::Loop => Self::Loop,
            PlayMode::Continue => Self::Continue,
        }
    }

    pub(super) fn next(self) -> Self {
        match self {
            Self::Default => Self::Loop,
            Self::Loop => Self::Continue,
            Self::Continue => Self::PlayAndHold,
            Self::PlayAndHold => Self::Default,
        }
    }

    pub(super) fn icon(self) -> icons::IconPair {
        match self {
            Self::Default => icons::PLAYMODE_DEFAULT,
            Self::Loop => icons::PLAYMODE_LOOP,
            Self::Continue => icons::PLAYMODE_CONTINUE,
            Self::PlayAndHold => icons::TRIGGER_HOLD,
        }
    }

    pub(super) fn tooltip(self) -> &'static str {
        match self {
            Self::Default => "Play Mode: Default",
            Self::Loop => "Play Mode: Loop",
            Self::Continue => "Play Mode: Continue",
            Self::PlayAndHold => "Play Mode: Play & Hold - sounds play while the key is held",
        }
    }

    fn settings(self) -> (PlayMode, TriggerMode) {
        match self {
            Self::Default => (PlayMode::Default, TriggerMode::Stop),
            Self::Loop => (PlayMode::Loop, TriggerMode::Stop),
            Self::Continue => (PlayMode::Continue, TriggerMode::Stop),
            Self::PlayAndHold => (PlayMode::Default, TriggerMode::PlayAndHold),
        }
    }
}

pub(super) fn set_cycle_mode(
    mode: CycleMode,
    state: &Arc<AppState>,
) -> Result<(), commands::CommandError> {
    let (play_mode, trigger_mode) = mode.settings();
    commands::set_trigger_mode(trigger_mode.as_str().to_string(), Arc::clone(&state.config))?;
    commands::set_play_mode(
        play_mode.as_str().to_string(),
        Arc::clone(&state.config),
        Arc::clone(&state.player),
    )
}

pub(super) fn update_play_mode_button<T>(button: &T, mode: CycleMode)
where
    T: IsA<gtk4::Button> + IsA<gtk4::Widget>,
{
    icons::apply_button_icon(button, mode.icon());
    set_tooltip_if_changed(button, mode.tooltip());
    set_active_class(button, mode != CycleMode::Default);
}

impl TransportBar {
    pub fn toggle_play_pause(&self) {
        let track = self.inner.active_track.borrow();
        if let Some(track) = track.as_ref() {
            let positions = commands::get_playback_positions(self.inner.state.player.clone());
            let is_paused = positions
                .iter()
                .any(|position| position.sound_id == track.sound_id && position.paused);
            if is_paused {
                commands::resume_sound(track.sound_id.clone(), self.inner.state.player.clone());
                update_play_pause_button(&self.inner.play_btn, true);
            } else {
                commands::pause_sound(track.sound_id.clone(), self.inner.state.player.clone());
                update_play_pause_button(&self.inner.play_btn, false);
            }
        }
    }

    pub fn play_previous(&self) {
        self.inner.play_adjacent_sound(-1);
    }

    pub fn play_next(&self) {
        self.inner.play_adjacent_sound(1);
    }

    pub fn stop_all(&self) {
        self.inner.stop_all_playback();
    }

    pub fn toggle_headphones_mute(&self) {
        match commands::toggle_local_mute(
            Arc::clone(&self.inner.state.config),
            Arc::clone(&self.inner.state.player),
        ) {
            Ok(local_mute) => self.apply_headphones_state(local_mute),
            Err(e) => {
                log::warn!("Toggle local mute failed via hotkey: {e}");
                self.refresh_controls_from_state();
            }
        }
    }

    pub fn toggle_mic_mute(&self) {
        match commands::toggle_mic_passthrough(
            Arc::clone(&self.inner.state.config),
            Arc::clone(&self.inner.state.player),
        ) {
            Ok(enabled) => self.apply_mic_state(enabled),
            Err(e) => {
                log::warn!("Toggle mic passthrough failed via hotkey: {e}");
                self.refresh_controls_from_state();
            }
        }
    }

    pub fn cycle_play_mode(&self) {
        let new_mode = {
            let cfg = self.inner.state.config.lock();
            CycleMode::from_settings(cfg.settings.play_mode, cfg.settings.trigger_mode).next()
        };
        if let Err(e) = set_cycle_mode(new_mode, &self.inner.state) {
            log::warn!("Set play mode failed via hotkey: {e}");
            self.refresh_controls_from_state();
            return;
        }
        update_play_mode_button(&self.inner.playmode_btn, new_mode);
    }

    pub fn refresh_controls_from_state(&self) {
        let (local_mute, mic_passthrough, play_mode, trigger_mode) = {
            let cfg = self.inner.state.config.lock();
            (
                cfg.settings.local_mute,
                cfg.settings.mic_passthrough,
                cfg.settings.play_mode,
                cfg.settings.trigger_mode,
            )
        };
        self.apply_headphones_state(local_mute);
        self.apply_mic_state(mic_passthrough);
        update_play_mode_button(
            &self.inner.playmode_btn,
            CycleMode::from_settings(play_mode, trigger_mode),
        );
    }

    pub(super) fn apply_headphones_state(&self, local_mute: bool) {
        self.inner.suppress_headphones_toggle.set(true);
        set_active_if_changed(&self.inner.headphones_btn, !local_mute);
        self.inner.suppress_headphones_toggle.set(false);
        update_headphones_button(&self.inner.headphones_btn, !local_mute);
    }

    pub(super) fn apply_mic_state(&self, enabled: bool) {
        self.inner.suppress_mic_toggle.set(true);
        set_active_if_changed(&self.inner.mic_btn, enabled);
        self.inner.suppress_mic_toggle.set(false);
        update_mic_button(&self.inner.mic_btn, enabled);
    }
}

impl TransportInner {
    pub(super) fn has_navigation_sounds(&self) -> bool {
        let guard = self.has_sounds_checker.borrow();
        match guard.as_ref() {
            Some(checker) => checker(),
            None => false,
        }
    }

    pub(super) fn play_adjacent_sound(&self, offset: i32) {
        self.clear_continue_suppression();
        let provider = self.sound_list_provider.borrow();
        let context = match provider.as_ref() {
            Some(provider) => provider(),
            None => return,
        };

        commands::stop_all(self.state.player.clone());
        if let Err(error) = commands::play_adjacent_sound_async(
            context.scope,
            context.search,
            context.position,
            offset,
            Arc::clone(&self.state),
            move |result| {
                if let Err(error) = result {
                    log::warn!("Play adjacent failed: {error}");
                }
            },
        ) {
            log::warn!("Failed to dispatch adjacent playback: {error}");
        }
    }

    pub(super) fn remember_continue_cursor(&self, sound_id: &str) {
        let provider = self.sound_list_provider.borrow();
        let cursor = provider.as_ref().map(|provider| {
            let context = provider();
            ContinueCursor {
                sound_id: sound_id.to_string(),
                scope: context.scope,
                search: context.search,
            }
        });
        *self.continue_cursor.borrow_mut() = cursor;
    }

    pub(super) fn continue_to_next_sound(self: &Rc<Self>) -> bool {
        if self.continue_advance_pending.get() {
            return true;
        }
        let Some(cursor) = self.continue_cursor.borrow().clone() else {
            return false;
        };

        self.clear_continue_suppression();
        self.continue_advance_pending.set(true);
        commands::stop_all(self.state.player.clone());

        let weak = Rc::downgrade(self);
        match commands::play_adjacent_sound_from_id_async(
            cursor.scope,
            cursor.search,
            cursor.sound_id,
            1,
            Arc::clone(&self.state),
            move |result| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                if let Err(error) = result {
                    log::warn!("Continue playback failed: {error}");
                    inner.reset_idle_playback_ui();
                }
            },
        ) {
            Ok(()) => true,
            Err(error) => {
                self.continue_advance_pending.set(false);
                log::warn!("Failed to dispatch Continue playback: {error}");
                false
            }
        }
    }

    pub(super) fn stop_all_playback(&self) {
        *self.continue_suppressed_play_id.borrow_mut() = self
            .active_track
            .borrow()
            .as_ref()
            .map(|track| track.play_id.clone());
        commands::stop_all(self.state.player.clone());
    }

    pub(super) fn is_continue_suppressed(&self) -> bool {
        self.continue_suppressed_play_id.borrow().is_some()
    }

    pub(super) fn clear_continue_suppression(&self) {
        *self.continue_suppressed_play_id.borrow_mut() = None;
    }

    pub(super) fn clear_continue_suppression_for_playback(&self, play_id: &str) {
        let should_clear = should_clear_continue_suppression(
            self.continue_suppressed_play_id.borrow().as_deref(),
            play_id,
        );
        if should_clear {
            self.clear_continue_suppression();
        }
    }

    pub(super) fn reset_idle_playback_ui(&self) {
        self.play_btn.set_sensitive(false);
        update_play_pause_button(&self.play_btn, false);
        self.stop_btn.set_sensitive(false);
        self.prev_btn.set_sensitive(false);
        self.next_btn.set_sensitive(false);
        self.scrub.set_sensitive(false);
        self.track_name_label.set_visible(false);
        *self.active_track.borrow_mut() = None;
        *self.continue_cursor.borrow_mut() = None;
        self.continue_advance_pending.set(false);
        self.time_label.set_text("0:00");
        self.dur_label.set_text("0:00");
        crate::ui_event_bridge::post_now_playing(None);
    }
}
