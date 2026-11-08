pub mod audio;
pub mod client;
pub mod connection;
pub mod crypto;
pub mod dave;
pub mod gateway;
pub mod networking;
pub mod opus;
pub mod realtime;
pub mod types;
pub mod udp;
pub mod ws;

pub use client::VoiceClient;
pub use connection::VoiceConnection;
pub use types::{
    AudioPlayerStatus, BotOptions, ConnectionOptions, EncodedAudio, EncoderOptions, JoinConfig,
    PlaybackOptions, VoiceConnectionStatus, VoiceServerInfo, VoiceStateInfo,
};
