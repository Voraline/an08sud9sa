use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::connection::VoiceConnection;
use crate::types::{AudioPlayerStatus, EncodedAudio, VoiceConnectionStatus};

const MAX_MISSED_FRAMES: usize = 5;

pub struct AudioResource {
    audio: Arc<EncodedAudio>,
    cursor: usize,
    padding: usize,
    silence_remaining: i64,
}

impl AudioResource {
    pub fn new(audio: Arc<EncodedAudio>, padding: usize) -> Self {
        Self {
            audio,
            cursor: 0,
            padding,
            silence_remaining: -1,
        }
    }

    fn read_into(&mut self, out: &mut Vec<u8>) -> bool {
        if self.silence_remaining > 0 {
            self.silence_remaining -= 1;
            out.clear();
            out.extend_from_slice(EncodedAudio::silence_frame());
            return true;
        }

        if self.cursor < self.audio.frames.len() {
            out.clear();
            out.extend_from_slice(&self.audio.frames[self.cursor]);
            self.cursor += 1;
            return true;
        }

        if self.silence_remaining == -1 {
            self.silence_remaining = self.padding as i64;
            if self.silence_remaining > 0 {
                self.silence_remaining -= 1;
                out.clear();
                out.extend_from_slice(EncodedAudio::silence_frame());
                return true;
            }
        }

        false
    }

    /// Drop `frames` worth of audio without producing packets.
    pub fn skip(&mut self, frames: usize) {
        let mut left = frames;
        if left == 0 {
            return;
        }

        if self.silence_remaining > 0 {
            let dropped = left.min(self.silence_remaining as usize);
            self.silence_remaining -= dropped as i64;
            left -= dropped;
        }

        if left > 0 {
            let available = self.audio.frames.len().saturating_sub(self.cursor);
            let dropped = left.min(available);
            self.cursor += dropped;
            left -= dropped;
        }

        if left > 0 && self.silence_remaining == -1 && self.cursor >= self.audio.frames.len() {
            self.silence_remaining = self.padding.saturating_sub(left) as i64;
        }
    }
}

struct PlayerState {
    resource: Option<AudioResource>,
    missed_frames: usize,
    scratch: Vec<u8>,
}

pub struct AudioPlayer {
    connection: Arc<VoiceConnection>,
    state: Mutex<PlayerState>,
    status: AtomicU8,
}

impl AudioPlayer {
    pub fn new(connection: Arc<VoiceConnection>) -> Arc<Self> {
        Arc::new(Self {
            connection,
            state: Mutex::new(PlayerState {
                resource: None,
                missed_frames: 0,
                scratch: Vec::with_capacity(512),
            }),
            status: AtomicU8::new(AudioPlayerStatus::Idle as u8),
        })
    }

    pub fn play(&self, resource: AudioResource) {
        let mut state = self.state.lock();
        state.resource = Some(resource);
        state.missed_frames = 0;
        self.status
            .store(AudioPlayerStatus::Playing as u8, Ordering::Release);
    }

    pub fn stop(&self, _force: bool) -> bool {
        if self.status() == AudioPlayerStatus::Idle {
            return false;
        }
        {
            let mut state = self.state.lock();
            state.resource = None;
            state.missed_frames = 0;
        }
        self.set_status(AudioPlayerStatus::Idle);
        true
    }

    /// Move the cursor forward without producing packets.
    ///
    /// The lockstep pacer calls this when the shared clock outruns the host:
    /// every bot skips the same frames, so the content position is decided by
    /// the clock and never by a bot's private counter.
    pub fn advance(&self, frames: usize) {
        if frames == 0 {
            return;
        }
        let mut state = self.state.lock();
        let Some(resource) = state.resource.as_mut() else {
            return;
        };
        resource.skip(frames);
        state.missed_frames = 0;
    }

    pub fn status(&self) -> AudioPlayerStatus {
        match self.status.load(Ordering::Acquire) {
            0 => AudioPlayerStatus::Idle,
            1 => AudioPlayerStatus::Playing,
            2 => AudioPlayerStatus::Paused,
            _ => AudioPlayerStatus::AutoPaused,
        }
    }

    pub fn step_prepare(&self, sequence: u16, timestamp: u32) {
        let status = self.status();

        if status == AudioPlayerStatus::Idle || status == AudioPlayerStatus::Paused {
            return;
        }

        let playable = self.connection.status() == VoiceConnectionStatus::Ready;
        if status == AudioPlayerStatus::AutoPaused {
            if !playable {
                return;
            }
            self.set_status(AudioPlayerStatus::Playing);
        }

        if !playable {
            self.set_status(AudioPlayerStatus::AutoPaused);
            return;
        }

        let mut state = self.state.lock();
        let mut packet = std::mem::take(&mut state.scratch);
        let Some(resource) = state.resource.as_mut() else {
            state.scratch = packet;
            drop(state);
            self.set_status(AudioPlayerStatus::Idle);
            return;
        };

        let has_packet = resource.read_into(&mut packet);
        state.scratch = packet;

        if !has_packet {
            state.missed_frames += 1;
            if state.missed_frames >= MAX_MISSED_FRAMES {
                drop(state);
                self.set_status(AudioPlayerStatus::Idle);
                return;
            }
            self.connection.prepare_audio_packet_fast(
                EncodedAudio::silence_frame(),
                sequence,
                timestamp,
            );
            return;
        }

        state.missed_frames = 0;
        self.connection
            .prepare_audio_packet_fast(&state.scratch, sequence, timestamp);
    }

    fn set_status(&self, next: AudioPlayerStatus) {
        let previous = self.status.swap(next as u8, Ordering::AcqRel);
        let _ = previous;
    }
}
