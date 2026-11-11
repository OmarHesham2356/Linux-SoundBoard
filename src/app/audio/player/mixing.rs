use super::*;

pub(super) fn mix_tick(state_rc: &Rc<RefCell<LoopState>>) {
    let mut state = state_rc.borrow_mut();
    if !state.backend_playback_available() {
        state.available = false;
        state.publish_snapshot();
        return;
    }
    state.available = true;

    fill_output_queues(&mut state);

    let finished_ids = state
        .active_playbacks
        .iter()
        .filter(|(_, playback)| playback.finished)
        .map(|(play_id, _)| play_id.clone())
        .collect::<Vec<_>>();
    for play_id in finished_ids {
        if let Some(playback) = state.active_playbacks.remove(&play_id) {
            state.finished_playbacks.insert(
                play_id.clone(),
                PlaybackSnapshot {
                    sound_id: playback.sound_id.clone(),
                    playback_order: playback.playback_order,
                    position_ms: playback.position_ms,
                    paused: playback.paused,
                    duration_ms: playback.duration_ms,
                    finished: true,
                },
            );
            if state.solo_play_id.as_deref() == Some(play_id.as_str()) {
                state.solo_play_id = None;
            }
        }
    }
    state.trim_finished_playbacks(MAX_FINISHED_PLAYBACK_SNAPSHOTS);
    let active_ids = state.active_playbacks.keys().cloned().collect::<Vec<_>>();
    for play_id in active_ids {
        state.finished_playbacks.remove(&play_id);
    }
    if state
        .solo_play_id
        .as_ref()
        .is_some_and(|play_id| !state.active_playbacks.contains_key(play_id))
    {
        state.solo_play_id = None;
    }
    state.publish_snapshot();
}

pub(super) fn fill_output_queues(state: &mut LoopState) {
    let playback_active = !state.active_playbacks.is_empty();
    let capture_stream_active = state.capture_stream_active();
    let wants_local_output = playback_active;
    let wants_virtual_output =
        playback_active || (state.runtime.mic_passthrough && capture_stream_active);
    if !wants_local_output && !wants_virtual_output {
        state.ultra_starvation_ticks = 0;
        return;
    }

    trim_latency_backlog(state, wants_virtual_output);

    let local_target_samples = if wants_local_output {
        state.runtime.local_output_target_samples()
    } else {
        0
    };
    let virtual_target_samples = if wants_virtual_output {
        state.runtime.virtual_output_target_samples()
    } else {
        0
    };
    let max_fill_batches = state
        .runtime
        .max_fill_batches_per_tick(wants_local_output, wants_virtual_output);

    let fill_started_at = Instant::now();
    let mut batches = 0usize;
    while batches < max_fill_batches {
        let Some((local_deficit, virtual_deficit)) =
            current_queue_deficits(&state.queues, local_target_samples, virtual_target_samples)
        else {
            // RT owns the queue; retry next tick.
            state.stream_runtime.record_lock_contention();
            return;
        };
        let wanted_samples = local_deficit.max(virtual_deficit);
        if wanted_samples == 0 {
            break;
        }

        let chunk_samples = wanted_samples.min(MIX_CHUNK_FRAMES * TARGET_OUTPUT_CHANNELS as usize);
        let pushed = enqueue_mixed_chunk(state, chunk_samples);
        if pushed == 0 {
            break;
        }
        batches = batches.saturating_add(1);
    }

    if let Some((local_deficit, virtual_deficit)) =
        current_queue_deficits(&state.queues, local_target_samples, virtual_target_samples)
    {
        let needs_more_audio = local_deficit > 0 || virtual_deficit > 0;
        if needs_more_audio {
            let elapsed_ms = fill_started_at.elapsed().as_millis();
            if batches >= max_fill_batches {
                trace!(
                    "Mix fill budget exhausted: batches={} elapsed_ms={} local_deficit_samples={} virtual_deficit_samples={}",
                    batches,
                    elapsed_ms,
                    local_deficit,
                    virtual_deficit
                );
            }
            trace!(
                "Output queues remain short after fill: batches={} elapsed_ms={} local_deficit_samples={} virtual_deficit_samples={}",
                batches,
                elapsed_ms,
                local_deficit,
                virtual_deficit
            );
        }

        if state.runtime.mic_latency_profile == MicLatencyProfile::Ultra && wants_virtual_output {
            if needs_more_audio {
                state.ultra_starvation_ticks = state.ultra_starvation_ticks.saturating_add(1);
                if state.ultra_starvation_ticks >= ULTRA_STARVATION_TICK_FALLBACK_THRESHOLD {
                    warn!(
                        "Ultra mic latency profile is underrunning; falling back to low latency profile"
                    );
                    state.runtime.mic_latency_profile = MicLatencyProfile::Low;
                    state.stream_runtime.apply_runtime(&state.runtime);
                    state.ultra_starvation_ticks = 0;
                    clear_virtual_mic_queues(&state.queues);
                }
            } else {
                state.ultra_starvation_ticks = 0;
            }
        } else {
            state.ultra_starvation_ticks = 0;
        }
    }
}

fn trim_latency_backlog(state: &mut LoopState, wants_virtual_output: bool) {
    if !wants_virtual_output {
        return;
    }

    let max_virtual_backlog_samples = state.stream_runtime.max_virtual_backlog_samples();
    let max_mic_backlog_samples = state.stream_runtime.max_mic_backlog_samples();

    if let Some(mut queues) = state.queues.try_lock() {
        let dropped_virtual = queues
            .virtual_out
            .trim_oldest_to(max_virtual_backlog_samples);
        let dropped_mic = queues.mic_in.trim_oldest_to(max_mic_backlog_samples);
        if dropped_virtual > 0 || dropped_mic > 0 {
            debug!(
                "Dropped stale mic backlog: dropped_virtual_samples={} dropped_mic_samples={} profile={}",
                dropped_virtual,
                dropped_mic,
                state.runtime.mic_latency_profile.as_str()
            );
        }
    }
}

fn current_queue_deficits(
    queues: &RtSharedQueues,
    local_target_samples: usize,
    virtual_target_samples: usize,
) -> Option<(usize, usize)> {
    let queues = queues.try_lock()?;
    let local_deficit = local_target_samples.saturating_sub(queues.local.len());
    let virtual_deficit = virtual_target_samples.saturating_sub(queues.virtual_out.len());
    Some((local_deficit, virtual_deficit))
}

fn enqueue_mixed_chunk(state: &mut LoopState, chunk_samples: usize) -> usize {
    let runtime = state.runtime.clone();
    let playback_active = !state.active_playbacks.is_empty();
    let capture_stream_active = state.capture_stream_active();
    let passthrough_active = state.runtime.mic_passthrough && capture_stream_active;

    if passthrough_active && !playback_active {
        return if let Some(mut queues) = state.queues.try_lock() {
            enqueue_passthrough_chunk(&mut queues, chunk_samples)
        } else {
            // Do not block the main loop on the RT lock.
            state.stream_runtime.record_lock_contention();
            0
        };
    }

    if state.local_mix_buffer.len() != chunk_samples {
        state.local_mix_buffer.resize(chunk_samples, 0.0);
    } else {
        state.local_mix_buffer.fill(0.0);
    }
    if state.virtual_mix_buffer.len() != chunk_samples {
        state.virtual_mix_buffer.resize(chunk_samples, 0.0);
    } else {
        state.virtual_mix_buffer.fill(0.0);
    }

    render_active_playbacks(state, &runtime, chunk_samples);

    // Allocate before taking the queue lock.
    if state.mic_scratch_buffer.len() < chunk_samples {
        state.mic_scratch_buffer.resize(chunk_samples, 0.0);
    }

    let Some(mut queues) = state.queues.try_lock() else {
        state.stream_runtime.record_lock_contention();
        return 0;
    };

    if passthrough_active && queues.mic_in.len() >= chunk_samples {
        let slot = &mut state.mic_scratch_buffer[..chunk_samples];
        let dequeued = queues.mic_in.pop_into(slot);
        for (virtual_sample, mic_sample) in
            state.virtual_mix_buffer.iter_mut().zip(&slot[..dequeued])
        {
            *virtual_sample = (*virtual_sample + *mic_sample).clamp(-1.0, 1.0);
        }
    }

    if playback_active {
        queues.local.push_slice(&state.local_mix_buffer);
    }
    if playback_active || passthrough_active {
        queues.virtual_out.push_slice(&state.virtual_mix_buffer);
    }

    chunk_samples
}

/// Mixes every audible playback into the shared output buffers.
fn render_active_playbacks(state: &mut LoopState, runtime: &RuntimeConfig, chunk_samples: usize) {
    let solo_play_id = state.audible_solo_play_id().map(str::to_string);
    let play_ids = state.active_playbacks.keys().cloned().collect::<Vec<_>>();

    for play_id in play_ids {
        if solo_play_id.as_deref().is_some_and(|solo| solo != play_id) {
            continue;
        }

        if state.local_playback_buffer.len() != chunk_samples {
            state.local_playback_buffer.resize(chunk_samples, 0.0);
        } else {
            state.local_playback_buffer.fill(0.0);
        }
        if state.virtual_playback_buffer.len() != chunk_samples {
            state.virtual_playback_buffer.resize(chunk_samples, 0.0);
        } else {
            state.virtual_playback_buffer.fill(0.0);
        }

        let Some(playback) = state.active_playbacks.get_mut(&play_id) else {
            continue;
        };
        playback.render_into(
            &mut state.local_playback_buffer,
            &mut state.virtual_playback_buffer,
            runtime,
        );

        if runtime.mute_voice {
            suppress_center_channel(&mut state.local_playback_buffer);
            suppress_center_channel(&mut state.virtual_playback_buffer);
        }

        for (mixed, sample) in state
            .local_mix_buffer
            .iter_mut()
            .zip(&state.local_playback_buffer)
        {
            *mixed = (*mixed + *sample).clamp(-1.0, 1.0);
        }
        for (mixed, sample) in state
            .virtual_mix_buffer
            .iter_mut()
            .zip(&state.virtual_playback_buffer)
        {
            *mixed = (*mixed + *sample).clamp(-1.0, 1.0);
        }
    }
}

/// Removes the centred vocal layer from interleaved stereo audio.
///
/// Tracks with identical left/right samples carry no stereo information to
/// separate, so those frames are left untouched instead of being silenced.
pub(super) fn suppress_center_channel(samples: &mut [f32]) {
    let has_stereo_content = samples
        .chunks_exact(2)
        .any(|frame| (frame[0] - frame[1]).abs() > 1e-6);
    if !has_stereo_content {
        return;
    }

    for frame in samples.chunks_exact_mut(2) {
        let center = (frame[0] + frame[1]) * 0.5;
        frame[0] = (frame[0] - center).clamp(-1.0, 1.0);
        frame[1] = (frame[1] - center).clamp(-1.0, 1.0);
    }
}

pub(super) fn enqueue_passthrough_chunk(queues: &mut ProcessQueues, chunk_samples: usize) -> usize {
    if queues.mic_in.len() < chunk_samples {
        return 0;
    }
    let mut samples = vec![0.0; chunk_samples];
    let dequeued = queues.mic_in.pop_into(&mut samples);
    queues.virtual_out.push_slice(&samples[..dequeued]);
    dequeued
}

pub(super) fn clear_virtual_mic_queues(queues: &RtSharedQueues) {
    // A later trim clears a contended queue.
    if let Some(mut queues) = queues.try_lock() {
        queues.mic_in.samples.clear();
        queues.virtual_out.samples.clear();
    }
}

pub(super) fn clear_mic_input_queue(queues: &RtSharedQueues) {
    if let Some(mut queues) = queues.try_lock() {
        queues.mic_in.samples.clear();
    }
}

pub(super) fn clear_all_queues(queues: &RtSharedQueues) {
    if let Some(mut queues) = queues.try_lock() {
        queues.local.samples.clear();
        queues.virtual_out.samples.clear();
        queues.mic_in.samples.clear();
    }
}

// ~5 ms at 48 kHz stereo — enough to ramp to silence without audible delay
const FADE_OUT_SAMPLES: usize = 480;

/// Replaces queued output with a short fade to zero.
pub(super) fn fade_output_queues(queues: &RtSharedQueues) {
    if let Some(mut queues) = queues.try_lock() {
        apply_fade_out(&mut queues.local);
        apply_fade_out(&mut queues.virtual_out);
    }
}

pub(super) fn apply_fade_out(queue: &mut SampleQueue) {
    if queue.samples.is_empty() {
        return;
    }
    // Trim all but the last FADE_OUT_SAMPLES so the stop is immediate.
    let len = queue.samples.len();
    if len > FADE_OUT_SAMPLES {
        queue.samples.drain(..len - FADE_OUT_SAMPLES);
    }
    // Apply linear ramp: index 0 → full amplitude, last index → 0.0
    let total = queue.samples.len();
    for (i, sample) in queue.samples.iter_mut().enumerate() {
        let scale = 1.0 - (i as f32 / (total - 1).max(1) as f32);
        *sample *= scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_queue(values: &[f32]) -> SampleQueue {
        let mut q = SampleQueue::new(4096);
        q.push_slice(values);
        q
    }

    #[test]
    fn fade_out_empty_queue_is_noop() {
        let mut q = SampleQueue::new(4096);
        apply_fade_out(&mut q);
        assert_eq!(q.samples.len(), 0);
    }

    #[test]
    fn fade_out_two_samples_last_becomes_zero() {
        let mut q = make_queue(&[0.8, 0.8]);
        apply_fade_out(&mut q);
        let samples: Vec<f32> = q.samples.iter().copied().collect();
        assert_eq!(samples.len(), 2);
        assert!(
            (samples[0] - 0.8).abs() < 1e-6,
            "first sample={}",
            samples[0]
        );
        assert!(samples[1].abs() < 1e-6, "last sample={}", samples[1]);
    }

    #[test]
    fn fade_out_long_queue_trims_to_fade_window() {
        let input = vec![1.0f32; 1000];
        let mut q = make_queue(&input);
        apply_fade_out(&mut q);
        assert_eq!(q.samples.len(), FADE_OUT_SAMPLES);
        let first = *q.samples.front().unwrap();
        let last = *q.samples.back().unwrap();
        assert!((first - 1.0).abs() < 1e-6, "first={}", first);
        assert!(last.abs() < 1e-6, "last={}", last);
    }

    #[test]
    fn suppress_center_channel_removes_centered_vocals_and_keeps_sides() {
        // [center, center, left-only, right-only]
        let mut samples = [0.5f32, 0.5, 0.8, -0.2];
        suppress_center_channel(&mut samples);

        assert!(samples[0].abs() < 1e-6);
        assert!(samples[1].abs() < 1e-6);
        assert!((samples[2] - 0.5).abs() < 1e-6);
        assert!((samples[3] + 0.5).abs() < 1e-6);
    }

    #[test]
    fn suppress_center_channel_leaves_identical_mono_frames_audible() {
        let mut samples = [0.4f32, 0.4, -0.3, -0.3];
        suppress_center_channel(&mut samples);

        assert_eq!(samples, [0.4, 0.4, -0.3, -0.3]);
    }

    #[test]
    fn passthrough_chunk_returns_zero_when_mic_empty() {
        let mut queues = ProcessQueues::new(4096, 4096, 4096);
        let result = enqueue_passthrough_chunk(&mut queues, 128);
        assert_eq!(result, 0);
        assert_eq!(queues.virtual_out.len(), 0);
    }

    #[test]
    fn passthrough_chunk_transfers_when_mic_has_enough() {
        let mut queues = ProcessQueues::new(4096, 4096, 4096);
        queues.mic_in.push_slice(&[0.5f32; 256]);
        let result = enqueue_passthrough_chunk(&mut queues, 128);
        assert_eq!(result, 128);
        assert_eq!(queues.virtual_out.len(), 128);
        assert_eq!(queues.mic_in.len(), 128);
    }

    #[test]
    fn passthrough_chunk_returns_zero_when_mic_shorter_than_chunk() {
        let mut queues = ProcessQueues::new(4096, 4096, 4096);
        queues.mic_in.push_slice(&[0.5f32; 64]);
        let result = enqueue_passthrough_chunk(&mut queues, 128);
        assert_eq!(result, 0);
        assert_eq!(queues.virtual_out.len(), 0);
        assert_eq!(queues.mic_in.len(), 64, "mic_in should be untouched");
    }
}
