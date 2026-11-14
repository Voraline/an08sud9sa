use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::networking::Networking;
use crate::types::{
    ConnectionOptions, JoinConfig, VoiceConnectionStatus,
};

type ReadyCallback = Arc<dyn Fn() + Send + Sync>;
type CodeCallback = Arc<dyn Fn(i32) + Send + Sync>;
type MessageCallback = Arc<dyn Fn(String) + Send + Sync>;

pub struct ConnectionState {
    status: AtomicU8,
    on_ready: Mutex<Option<ReadyCallback>>,
    on_close: Mutex<Option<CodeCallback>>,
    on_error: Mutex<Option<MessageCallback>>,
    on_debug: Mutex<Option<MessageCallback>>,
}

impl ConnectionState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            status: AtomicU8::new(VoiceConnectionStatus::Signalling as u8),
            on_ready: Mutex::new(None),
            on_close: Mutex::new(None),
            on_error: Mutex::new(None),
            on_debug: Mutex::new(None),
        })
    }

    pub fn status(&self) -> VoiceConnectionStatus {
        VoiceConnectionStatus::from_u8(self.status.load(Ordering::Acquire))
    }

    pub fn net_ready(&self) {
        self.status
            .store(VoiceConnectionStatus::Ready as u8, Ordering::Release);
        let callback = self.on_ready.lock().clone();
        if let Some(callback) = callback {
            callback();
        }
    }

    pub fn net_closed(&self, code: i32) {
        self.status
            .store(VoiceConnectionStatus::Disconnected as u8, Ordering::Release);
        let callback = self.on_close.lock().clone();
        if let Some(callback) = callback {
            callback(code);
        }
    }

    pub fn emit_error(&self, message: String) {
        let callback = self.on_error.lock().clone();
        if let Some(callback) = callback {
            callback(message);
        }
    }

    pub fn debug(&self, message: String) {
        let callback = self.on_debug.lock().clone();
        if let Some(callback) = callback {
            callback(message);
        }
    }
}

pub struct VoiceConnection {
    network: Arc<Networking>,
    state: Arc<ConnectionState>,
}

impl VoiceConnection {
    pub fn new(
        _join: JoinConfig,
        options: ConnectionOptions,
        debug: bool,
    ) -> Arc<Self> {
        let state = ConnectionState::new();
        let network = Arc::new(Networking::new(options, debug, state.clone()));
        Arc::new(Self { network, state })
    }

    pub fn start(&self) -> bool {
        if self.status() == VoiceConnectionStatus::Destroyed {
            return false;
        }
        self.state
            .status
            .store(VoiceConnectionStatus::Connecting as u8, Ordering::Release);
        self.network.start()
    }

    pub fn status(&self) -> VoiceConnectionStatus {
        self.state.status()
    }

    pub fn prepare_audio_packet_fast(
        &self,
        packet: &[u8],
        sequence: u16,
        timestamp: u32,
    ) -> bool {
        if self.status() != VoiceConnectionStatus::Ready {
            return false;
        }
        self.network
            .prepare_audio_packet_fast(packet, sequence, timestamp)
    }

    pub fn dispatch_audio(&self) -> bool {
        if self.status() != VoiceConnectionStatus::Ready {
            return false;
        }
        self.network.dispatch_audio()
    }

    pub fn txtime_active(&self) -> bool {
        self.status() == VoiceConnectionStatus::Ready && self.network.txtime_active()
    }

    pub fn txtime_errors(&self) -> (u64, i32) {
        self.network.txtime_errors()
    }

    pub fn dispatch_audio_at(&self, txtime_ns: u64) -> bool {
        if self.status() != VoiceConnectionStatus::Ready {
            return false;
        }
        self.network.dispatch_audio_at(txtime_ns)
    }

    pub fn set_speaking(&self, speaking: bool) {
        if self.status() != VoiceConnectionStatus::Ready {
            return;
        }
        self.network.set_speaking(speaking);
    }

    pub fn dave_ready(&self) -> bool {
        self.status() == VoiceConnectionStatus::Ready && self.network.dave_ready()
    }

    pub fn destroy(&self) {
        self.state
            .status
            .store(VoiceConnectionStatus::Destroyed as u8, Ordering::Release);
        self.network.destroy();
    }

    pub fn set_on_ready<F>(&self, callback: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self.state.on_ready.lock() = Some(Arc::new(callback));
    }

    pub fn set_on_close<F>(&self, callback: F)
    where
        F: Fn(i32) + Send + Sync + 'static,
    {
        *self.state.on_close.lock() = Some(Arc::new(callback));
    }

    pub fn set_on_error<F>(&self, callback: F)
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        *self.state.on_error.lock() = Some(Arc::new(callback));
    }

    pub fn set_on_debug<F>(&self, callback: F)
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        *self.state.on_debug.lock() = Some(Arc::new(callback));
    }
}
