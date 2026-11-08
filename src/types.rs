use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct JoinConfig {
    pub guild_id: String,
    pub channel_id: String,
    pub self_deaf: bool,
    pub self_mute: bool,
    pub group: String,
}

#[derive(Clone, Debug, Default)]
pub struct BotOptions {
    pub token: String,
    pub name: String,
    pub self_bot: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ConnectionOptions {
    pub server_id: String,
    pub channel_id: String,
    pub user_id: String,
    pub session_id: String,
    pub token: String,
    pub endpoint: String,
}

#[derive(Clone, Debug, Default)]
pub struct VoiceServerInfo {
    pub token: String,
    pub guild_id: String,
    pub endpoint: String,
}

#[derive(Clone, Debug, Default)]
pub struct VoiceStateInfo {
    pub session_id: String,
    pub channel_id: String,
    pub user_id: String,
    pub guild_id: String,
}

#[derive(Clone, Debug)]
pub struct EncoderOptions {
    pub bitrate: u32,
    pub complexity: u8,
    pub ffmpeg_path: String,
    pub debug: bool,
}

impl Default for EncoderOptions {
    fn default() -> Self {
        Self {
            bitrate: 128_000,
            complexity: 10,
            ffmpeg_path: "ffmpeg".to_string(),
            debug: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PlaybackOptions {
    pub lead_time: Duration,
    pub connect_timeout: Duration,
    pub connect_stagger: Duration,
    pub silence_padding_frames: usize,
    pub warmup_frames: usize,
    pub parallel_threshold: usize,
    pub bitrate: u32,
    pub opus_complexity: u8,
    pub ffmpeg_path: String,
    pub debug: bool,
}

impl Default for PlaybackOptions {
    fn default() -> Self {
        Self {
            lead_time: Duration::from_millis(2_000),
            connect_timeout: Duration::from_millis(60_000),
            connect_stagger: Duration::from_millis(750),
            silence_padding_frames: 5,
            warmup_frames: 5,
            parallel_threshold: 4,
            bitrate: 128_000,
            opus_complexity: 10,
            ffmpeg_path: "ffmpeg".to_string(),
            debug: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EncodedAudio {
    pub frames: Vec<Vec<u8>>,
    pub sample_rate: u32,
    pub channels: u8,
    pub frame_duration_ms: u32,
}

impl EncodedAudio {
    pub fn silence_frame() -> &'static [u8] {
        static SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];
        &SILENCE
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum VoiceConnectionStatus {
    Signalling = 0,
    Connecting = 1,
    Ready = 2,
    Disconnected = 3,
    Destroyed = 4,
}

impl VoiceConnectionStatus {
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Signalling,
            1 => Self::Connecting,
            2 => Self::Ready,
            3 => Self::Disconnected,
            _ => Self::Destroyed,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AudioPlayerStatus {
    Idle = 0,
    Playing = 1,
    Paused = 2,
    AutoPaused = 3,
}
