use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::audio::{AudioPlayer, AudioResource};
use crate::connection::VoiceConnection;
use crate::crypto;
use crate::gateway::GatewayClient;
use crate::opus;
use crate::realtime::{self, FALLBACK_SPIN_WINDOW, REALTIME_SPIN_WINDOW};
use crate::sync;
use crate::types::{
    AudioPlayerStatus, BotOptions, EncodedAudio, EncoderOptions, JoinConfig, PlaybackOptions,
    VoiceConnectionStatus, VoiceServerInfo, VoiceStateInfo,
};

pub type ErrorHandler = Arc<dyn Fn(&str, &str) + Send + Sync>;
pub type DebugHandler = Arc<dyn Fn(&str) + Send + Sync>;
pub type FinishedHandler = Arc<dyn Fn() + Send + Sync>;

struct BotSlot {
    connection: Option<Arc<VoiceConnection>>,
    player: Option<Arc<AudioPlayer>>,
}

struct Bot {
    options: BotOptions,
    gateway: Arc<GatewayClient>,
    slot: Mutex<BotSlot>,
    ready: AtomicBool,
    generation: AtomicU64,
}

struct ClientState {
    ready_count: usize,
    failed_count: usize,
    finished: bool,
    error: String,
}

struct Connector {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

#[derive(Clone)]
struct Playback {
    name: String,
    connection: Arc<VoiceConnection>,
    player: Arc<AudioPlayer>,
}

/// One bot as the lockstep pacer sees it. Warm-up frames are sent as Opus
/// silence without touching the player cursor, so the pre-roll never consumes
/// track audio.
struct PlaybackSink {
    connection: Arc<VoiceConnection>,
    player: Arc<AudioPlayer>,
    warmup: usize,
}

impl sync::Sink for PlaybackSink {
    fn prepare(&self, frame: usize, sequence: u16, timestamp: u32) {
        if frame < self.warmup {
            self.connection.prepare_audio_packet_fast(
                EncodedAudio::silence_frame(),
                sequence,
                timestamp,
            );
        } else {
            self.player.step_prepare(sequence, timestamp);
        }
    }

    fn dispatch(&self) -> bool {
        self.connection.dispatch_audio()
    }

    fn advance(&self, frames: usize) {
        self.player.advance(frames);
    }

    fn finished(&self) -> bool {
        self.player.status() == AudioPlayerStatus::Idle
    }
}

struct ClientInner {
    config: JoinConfig,
    settings: PlaybackOptions,
    bots: Vec<Arc<Bot>>,
    state: Mutex<ClientState>,
    condition: Condvar,
    stopping: AtomicBool,
    playing: AtomicBool,
    shutdown: AtomicBool,
    scheduler: Mutex<Option<JoinHandle<()>>>,
    connector: Mutex<Option<Connector>>,
    on_error: Mutex<Option<ErrorHandler>>,
    on_debug: Mutex<Option<DebugHandler>>,
    on_finished: Mutex<Option<FinishedHandler>>,
}

pub struct VoiceClient {
    inner: Arc<ClientInner>,
}

impl VoiceClient {
    pub fn new(
        bots: Vec<BotOptions>,
        config: JoinConfig,
        settings: PlaybackOptions,
    ) -> Self {
        let inner = Arc::new_cyclic(|weak: &Weak<ClientInner>| {
            let mut built = Vec::new();
            for (index, mut options) in bots.into_iter().enumerate() {
                if options.name.is_empty() {
                    options.name = format!("Bot{}", index + 1);
                }

                let gateway = GatewayClient::new(
                    options.token.clone(),
                    options.self_bot,
                    settings.debug,
                );

                {
                    let weak = weak.clone();
                    gateway.set_on_ready(move || {
                        if let Some(client) = weak.upgrade() {
                            client.gateway_ready(index);
                        }
                    });
                }
                {
                    let weak = weak.clone();
                    gateway.set_on_voice_ready(move |server, state| {
                        if let Some(client) = weak.upgrade() {
                            client.gateway_voice_ready(index, server, state);
                        }
                    });
                }
                {
                    let weak = weak.clone();
                    gateway.set_on_close(move |code| {
                        if let Some(client) = weak.upgrade() {
                            client.gateway_closed(index, code);
                        }
                    });
                }
                {
                    let weak: Weak<ClientInner> = weak.clone();
                    gateway.set_on_error(move |message| {
                        if let Some(client) = weak.upgrade() {
                            client.report(index, message);
                        }
                    });
                }
                {
                    let weak: Weak<ClientInner> = weak.clone();
                    gateway.set_on_debug(move |message| {
                        if let Some(client) = weak.upgrade() {
                            client.debug_bot(index, message);
                        }
                    });
                }

                built.push(Arc::new(Bot {
                    options,
                    gateway,
                    slot: Mutex::new(BotSlot {
                        connection: None,
                        player: None,
                    }),
                    ready: AtomicBool::new(false),
                    generation: AtomicU64::new(0),
                }));
            }

            ClientInner {
                config,
                settings,
                bots: built,
                state: Mutex::new(ClientState {
                    ready_count: 0,
                    failed_count: 0,
                    finished: true,
                    error: String::new(),
                }),
                condition: Condvar::new(),
                stopping: AtomicBool::new(false),
                playing: AtomicBool::new(false),
                shutdown: AtomicBool::new(false),
                scheduler: Mutex::new(None),
                connector: Mutex::new(None),
                on_error: Mutex::new(None),
                on_debug: Mutex::new(None),
                on_finished: Mutex::new(None),
            }
        });

        realtime::init_timer_resolution();
        Self { inner }
    }

    pub fn start(&self) -> bool {
        self.inner.clone().start()
    }

    pub fn play_file(&self, file: &Path) -> Result<(), String> {
        let encoder = EncoderOptions {
            bitrate: self.inner.settings.bitrate,
            complexity: self.inner.settings.opus_complexity,
            ffmpeg_path: self.inner.settings.ffmpeg_path.clone(),
            debug: self.inner.settings.debug,
        };
        let audio = Arc::new(opus::encode_file(file, &encoder)?);
        self.inner.clone().play(audio)
    }

    pub fn play(&self, audio: Arc<EncodedAudio>) -> Result<(), String> {
        self.inner.clone().play(audio)
    }

    pub fn stop(&self) {
        self.inner.stop();
    }

    pub fn disconnect(&self) {
        self.inner.disconnect();
    }

    pub fn wait_for_finished(&self) {
        self.inner.wait_for_finished();
    }

    pub fn wait_for_ready(&self, timeout: Duration) -> bool {
        self.inner.wait_for_ready(timeout)
    }

    pub fn ready_count(&self) -> usize {
        self.inner.state.lock().ready_count
    }

    pub fn bot_count(&self) -> usize {
        self.inner.bots.len()
    }

    pub fn last_error(&self) -> String {
        self.inner.state.lock().error.clone()
    }

    pub fn set_on_error<F>(&self, callback: F)
    where
        F: Fn(&str, &str) + Send + Sync + 'static,
    {
        *self.inner.on_error.lock() = Some(Arc::new(callback));
    }

    pub fn set_on_debug<F>(&self, callback: F)
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        *self.inner.on_debug.lock() = Some(Arc::new(callback));
    }

    pub fn set_on_finished<F>(&self, callback: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        *self.inner.on_finished.lock() = Some(Arc::new(callback));
    }
}

impl Drop for VoiceClient {
    fn drop(&mut self) {
        self.inner.disconnect();
        realtime::cleanup_timer_resolution();
    }
}

impl ClientInner {
    fn report(&self, index: usize, message: String) {
        let name = self.bots[index].options.name.clone();
        self.report_named(&name, &message);
    }

    fn report_named(&self, name: &str, message: &str) {
        self.state.lock().error = format!("{}: {}", name, message);
        let callback = self.on_error.lock().clone();
        if let Some(callback) = callback {
            callback(name, message);
        }
    }

    fn log(&self, message: String) {
        if !self.settings.debug {
            return;
        }
        let callback = self.on_debug.lock().clone();
        if let Some(callback) = callback {
            callback(&message);
        }
    }

    fn debug_bot(&self, index: usize, message: String) {
        if !self.settings.debug {
            return;
        }
        self.log(format!("[{}] {}", self.bots[index].options.name, message));
    }

    fn gateway_ready(self: &Arc<Self>, index: usize) {
        self.bots[index].gateway.join(&self.config);
    }

    fn gateway_voice_ready(
        self: Arc<Self>,
        index: usize,
        server: VoiceServerInfo,
        state: VoiceStateInfo,
    ) {
        let client = self.clone();
        thread::spawn(move || client.create_connection(index, server, state));
    }

    fn gateway_closed(&self, index: usize, code: i32) {
        if code != 0 && code != 1000 {
            self.report(
                index,
                format!("Gateway closed with code {}", code),
            );
        }
        self.state.lock().failed_count += 1;
        self.condition.notify_all();
    }

    fn create_connection(
        self: Arc<Self>,
        index: usize,
        server: VoiceServerInfo,
        state: VoiceStateInfo,
    ) {
        if self.shutdown.load(Ordering::Acquire) || self.playing.load(Ordering::Acquire) {
            self.debug_bot(index, "ignoring redundant voice session update".to_string());
            return;
        }

        let bot = &self.bots[index];
        if bot.ready.load(Ordering::Acquire) {
            self.debug_bot(index, "ignoring redundant voice session update".to_string());
            return;
        }

        let generation = bot.generation.fetch_add(1, Ordering::AcqRel) + 1;

        let (previous_connection, previous_player) = {
            let mut slot = bot.slot.lock();
            (slot.connection.take(), slot.player.take())
        };
        if let Some(connection) = previous_connection {
            connection.destroy();
        }
        drop(previous_player);

        let was_ready = bot.ready.swap(false, Ordering::AcqRel);
        if was_ready {
            let mut client_state = self.state.lock();
            if client_state.ready_count > 0 {
                client_state.ready_count -= 1;
            }
        }

        let options = crate::types::ConnectionOptions {
            server_id: self.config.guild_id.clone(),
            channel_id: self.config.channel_id.clone(),
            user_id: bot.gateway.user_id(),
            session_id: state.session_id,
            token: server.token,
            endpoint: server.endpoint,
        };

        let connection = VoiceConnection::new(self.config.clone(), options, self.settings.debug);
        {
            let weak = Arc::downgrade(&self);
            connection.set_on_ready(move || {
                if let Some(client) = weak.upgrade() {
                    client.connection_ready(index, generation);
                }
            });
        }
        {
            let weak = Arc::downgrade(&self);
            connection.set_on_close(move |code| {
                if let Some(client) = weak.upgrade() {
                    client.connection_closed(index, generation, code);
                }
            });
        }
        {
            let weak = Arc::downgrade(&self);
            connection.set_on_error(move |message| {
                if let Some(client) = weak.upgrade() {
                    client.report(index, message);
                }
            });
        }
        {
            let weak = Arc::downgrade(&self);
            connection.set_on_debug(move |message| {
                if let Some(client) = weak.upgrade() {
                    client.debug_bot(index, message);
                }
            });
        }

        let player = AudioPlayer::new(connection.clone());
        {
            let mut slot = bot.slot.lock();
            slot.connection = Some(connection.clone());
            slot.player = Some(player);
        }

        thread::spawn(move || {
            connection.start();
        });
    }

    fn connection_ready(&self, index: usize, generation: u64) {
        let bot = &self.bots[index];
        if bot.generation.load(Ordering::Acquire) != generation {
            return;
        }
        if !bot.ready.swap(true, Ordering::AcqRel) {
            self.state.lock().ready_count += 1;
            self.debug_bot(index, "voice ready".to_string());
        }
        self.condition.notify_all();
    }

    fn connection_closed(&self, index: usize, generation: u64, code: i32) {
        let bot = &self.bots[index];
        if bot.generation.load(Ordering::Acquire) != generation {
            return;
        }

        let was_ready = bot.ready.swap(false, Ordering::AcqRel);
        {
            let mut client_state = self.state.lock();
            if was_ready {
                if client_state.ready_count > 0 {
                    client_state.ready_count -= 1;
                }
            } else {
                client_state.failed_count += 1;
            }
        }

        if code == 4006 {
            self.report(
                index,
                "Voice session invalidated (4006); another client/session likely took over this account"
                    .to_string(),
            );
        } else if code != 0 && code != 1000 {
            self.report(index, format!("Voice connection closed with code {}", code));
        } else {
            self.debug_bot(index, "voice connection closed".to_string());
        }
        self.condition.notify_all();
    }

    fn start(self: Arc<Self>) -> bool {
        if self.bots.is_empty() {
            self.report_named("client", "No bots configured");
            return false;
        }

        self.shutdown.store(false, Ordering::Release);
        self.stop_connector();

        {
            let mut state = self.state.lock();
            state.ready_count = 0;
            state.failed_count = 0;
            state.error.clear();
        }
        for bot in &self.bots {
            bot.ready.store(false, Ordering::Release);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let client = self.clone();
        let stop_thread = stop.clone();
        let handle = thread::spawn(move || {
            for (index, bot) in client.bots.iter().enumerate() {
                if stop_thread.load(Ordering::Relaxed) {
                    return;
                }
                bot.gateway.connect();
                if index + 1 < client.bots.len() {
                    let deadline = Instant::now() + client.settings.connect_stagger;
                    while Instant::now() < deadline && !stop_thread.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(25));
                    }
                }
            }
        });
        *self.connector.lock() = Some(Connector { stop, handle });

        let count = self.bots.len();
        let mut state = self.state.lock();
        let deadline = Instant::now() + self.settings.connect_timeout;
        loop {
            if state.ready_count >= count || state.ready_count + state.failed_count >= count {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            self.condition.wait_for(&mut state, deadline - now);
        }

        let ready = state.ready_count >= count;
        let no_error = state.error.is_empty();
        drop(state);

        if !ready && no_error {
            let first = &self.bots[0];
            let gateway_ready = first.gateway.is_ready();
            let voice_packets = first.gateway.is_voice_ready();
            let message = if !gateway_ready {
                "Timed out waiting for gateway READY (invalid token or selfbot mode mismatch)"
                    .to_string()
            } else if !voice_packets {
                "Gateway READY, but no voice server/state packets (check guild id, channel id, permissions)"
                    .to_string()
            } else {
                "Voice gateway/UDP handshake did not complete".to_string()
            };
            self.state.lock().error = message;
        }

        ready
    }

    fn wait_for_ready(&self, timeout: Duration) -> bool {
        let count = self.bots.len();
        let mut state = self.state.lock();
        let deadline = Instant::now() + timeout;
        loop {
            if state.ready_count >= count || state.ready_count + state.failed_count >= count {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            self.condition.wait_for(&mut state, deadline - now);
        }
        state.ready_count >= count
    }

    fn stop_connector(&self) {
        if let Some(connector) = self.connector.lock().take() {
            connector.stop.store(true, Ordering::Relaxed);
            if connector.handle.thread().id() != thread::current().id() {
                let _ = connector.handle.join();
            }
        }
    }

    fn play(self: Arc<Self>, audio: Arc<EncodedAudio>) -> Result<(), String> {
        if audio.frames.is_empty() {
            return Err("Audio resource is empty".to_string());
        }

        self.stop();

        let active: Vec<Playback> = {
            let mut active = Vec::new();
            for bot in &self.bots {
                let slot = bot.slot.lock();
                if let (Some(connection), Some(player)) = (&slot.connection, &slot.player) {
                    if connection.status() == VoiceConnectionStatus::Ready {
                        active.push(Playback {
                            name: bot.options.name.clone(),
                            connection: connection.clone(),
                            player: player.clone(),
                        });
                    }
                }
            }
            if active.is_empty() {
                return Err("No ready voice connections".to_string());
            }
            active
        };

        {
            let mut state = self.state.lock();
            state.finished = false;
        }
        self.playing.store(true, Ordering::Release);

        let deadline = Instant::now() + Duration::from_secs(10);
        for target in &active {
            let mut warned = false;
            while !target.connection.dave_ready() {
                if !warned {
                    self.log(format!("[{}] waiting for DAVE", target.name));
                    warned = true;
                }
                if Instant::now() >= deadline {
                    self.playing.store(false, Ordering::Release);
                    return Err(format!("DAVE encryption is not ready for {}", target.name));
                }
                thread::sleep(Duration::from_millis(10));
            }
        }

        for target in &active {
            let resource = AudioResource::new(audio.clone(), self.settings.silence_padding_frames);
            target.player.play(resource);
            target.connection.set_speaking(true);
        }

        let client = self.clone();
        let playback_audio = audio.clone();
        let playback = active.clone();
        let handle = thread::spawn(move || client.run_scheduler(playback_audio, playback));
        *self.scheduler.lock() = Some(handle);

        Ok(())
    }

    fn wait_for_finished(&self) {
        let mut state = self.state.lock();
        while !state.finished {
            self.condition.wait(&mut state);
        }
    }

    fn stop(&self) {
        self.playing.store(false, Ordering::Release);
        self.stopping.store(true, Ordering::Release);
        self.condition.notify_all();

        let scheduler = self.scheduler.lock().take();
        if let Some(handle) = scheduler {
            if handle.thread().id() != thread::current().id() {
                let _ = handle.join();
            }
        }

        for bot in &self.bots {
            let slot = bot.slot.lock();
            if let Some(player) = &slot.player {
                player.stop(true);
            }
            if let Some(connection) = &slot.connection {
                if connection.status() == VoiceConnectionStatus::Ready {
                    connection.set_speaking(false);
                }
            }
        }

        {
            let mut state = self.state.lock();
            state.finished = true;
        }
        self.stopping.store(false, Ordering::Release);
        self.condition.notify_all();
    }

    fn disconnect(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.stop();
        self.stop_connector();

        for bot in &self.bots {
            let (connection, player) = {
                let mut slot = bot.slot.lock();
                (slot.connection.take(), slot.player.take())
            };
            if let Some(connection) = connection {
                connection.destroy();
            }
            drop(player);
            bot.gateway.leave();
            bot.gateway.close();
            bot.ready.store(false, Ordering::Release);
        }

        {
            let mut state = self.state.lock();
            state.ready_count = 0;
            state.failed_count = 0;
        }
        self.condition.notify_all();
    }

    fn run_scheduler(self: Arc<Self>, audio: Arc<EncodedAudio>, active: Vec<Playback>) {
        if active.is_empty() {
            self.playing.store(false, Ordering::Release);
            let mut state = self.state.lock();
            state.finished = true;
            drop(state);
            self.condition.notify_all();
            return;
        }

        let cpus = realtime::allowed_cpus();
        let locked = realtime::lock_memory();
        let pinned = cpus
            .last()
            .map(|cpu| realtime::pin_current_thread(*cpu))
            .unwrap_or(false);
        let realtime_ok = realtime::elevate_current_thread();
        let spin = if realtime_ok {
            REALTIME_SPIN_WINDOW
        } else {
            FALLBACK_SPIN_WINDOW
        };

        // The clock starts now. `lead_time` is spent as pre-roll silence, so
        // every listener stream is established before the first audible frame.
        let frame_duration = Duration::from_millis(sync::FRAME_MILLIS);
        let warmup = sync::preroll_frames(
            self.settings.lead_time,
            frame_duration,
            self.settings.warmup_frames,
        );
        let maximum_frames =
            warmup + audio.frames.len() + self.settings.silence_padding_frames + 100;

        let targets: Vec<sync::Target> = active
            .iter()
            .map(|playback| {
                sync::Target::new(
                    playback.name.clone(),
                    Arc::new(PlaybackSink {
                        connection: playback.connection.clone(),
                        player: playback.player.clone(),
                        warmup,
                    }),
                )
            })
            .collect();

        let config = sync::PacerConfig {
            frame_duration,
            maximum_frames,
            warmup_frames: warmup,
            base_sequence: crypto::random_u16(),
            base_timestamp: crypto::random_u32(),
            spin_window: spin,
            sample_capacity: sync::DEFAULT_SAMPLE_CAPACITY,
        };

        let report = sync::Pacer::new(config).run(&targets, &self.stopping, |message| {
            self.log(message);
        });

        self.log(format!(
            "[SYNC] bots={} pinned={} realtime={} locked={} warmup={} {}",
            active.len(),
            if pinned { 1 } else { 0 },
            if realtime_ok { 1 } else { 0 },
            if locked { 1 } else { 0 },
            warmup,
            report.summary()
        ));

        self.playing.store(false, Ordering::Release);
        for target in &active {
            target.connection.set_speaking(false);
            target.player.stop(true);
        }

        {
            let mut state = self.state.lock();
            state.finished = true;
        }
        self.stopping.store(false, Ordering::Release);
        self.condition.notify_all();

        let callback = self.on_finished.lock().clone();
        if let Some(callback) = callback {
            callback();
        }
    }
}
