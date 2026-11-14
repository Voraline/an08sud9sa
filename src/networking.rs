use std::collections::HashSet;
use std::sync::Arc;
use std::thread;

use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::connection::ConnectionState;
use crate::crypto;
use crate::dave::{Dave, EncryptOutcome, ProcessResult};
use crate::types::ConnectionOptions;
use crate::udp::{SocketConfig, UdpCallbacks, VoiceUdpSocket};
use crate::ws::{HeartbeatKind, WsClient, WsConn, WsEvent};

const ENCRYPTION_MODE: &str = "aead_xchacha20_poly1305_rtpsize";

#[derive(Clone, Copy, PartialEq, Eq)]
enum NetCode {
    Opening,
    Identifying,
    UdpHandshaking,
    SelectingProtocol,
    Ready,
    Closed,
}

struct NetShared {
    code: NetCode,
    ssrc: u32,
    mode: String,
    key: [u8; 32],
    sequence: u16,
    timestamp: u32,
    nonce: u32,
    prepared: Vec<u8>,
    dave_frame: Vec<u8>,
    speaking: bool,
    txtime: bool,
    connected: HashSet<String>,
    external_sender: Option<Vec<u8>>,
    dave: Dave,
    dave_version: u16,
    rtp_logged: bool,
    audio_send_logged: bool,
}

struct NetInner {
    options: ConnectionOptions,
    debug: bool,
    state: Arc<ConnectionState>,
    shared: Mutex<NetShared>,
    udp: Mutex<Option<Arc<VoiceUdpSocket>>>,
    conn: Mutex<Option<WsConn>>,
}

pub struct Networking {
    inner: Arc<NetInner>,
    ws: Mutex<Option<WsClient>>,
}

impl Networking {
    pub fn new(options: ConnectionOptions, debug: bool, state: Arc<ConnectionState>) -> Self {
        let shared = NetShared {
            code: NetCode::Opening,
            ssrc: 0,
            mode: String::new(),
            key: [0u8; 32],
            sequence: 0,
            timestamp: 0,
            nonce: 0,
            prepared: Vec::with_capacity(4096),
            dave_frame: Vec::with_capacity(4096),
            speaking: false,
            txtime: false,
            connected: HashSet::new(),
            external_sender: None,
            dave: Dave::new(),
            dave_version: 0,
            rtp_logged: false,
            audio_send_logged: false,
        };

        Self {
            inner: Arc::new(NetInner {
                options,
                debug,
                state,
                shared: Mutex::new(shared),
                udp: Mutex::new(None),
                conn: Mutex::new(None),
            }),
            ws: Mutex::new(None),
        }
    }

    pub fn start(&self) -> bool {
        if self.inner.shared.lock().code == NetCode::Closed {
            return false;
        }

        let mut endpoint = self.inner.options.endpoint.clone();
        if !endpoint.is_empty() && !endpoint.ends_with('/') {
            endpoint.push('/');
        }
        let url = format!("wss://{}?v=8", endpoint);

        if self.inner.debug {
            self.inner.state.debug(format!(
                "[NW] connecting to {} guild {} user {} session {}",
                self.inner.options.endpoint,
                self.inner.options.server_id,
                self.inner.options.user_id,
                self.inner.options.session_id
            ));
        }

        let inner = self.inner.clone();
        let handler: crate::ws::WsHandler =
            Arc::new(move |event, conn| handle_event(&inner, event, conn));

        match WsClient::connect(&url, HeartbeatKind::Voice, handler) {
            Ok(client) => {
                *self.inner.conn.lock() = Some(client.conn());
                *self.ws.lock() = Some(client);
                true
            }
            Err(error) => {
                self.inner
                    .state
                    .emit_error(format!("Voice WebSocket connect failed: {error}"));
                {
                    let mut shared = self.inner.shared.lock();
                    shared.code = NetCode::Closed;
                    shared.prepared.clear();
                }
                self.inner.state.net_closed(0);
                false
            }
        }
    }

    pub fn destroy(&self) {
        {
            let mut shared = self.inner.shared.lock();
            if shared.code == NetCode::Closed {
                return;
            }
            shared.code = NetCode::Closed;
            shared.prepared.clear();
        }
        if let Some(mut client) = self.ws.lock().take() {
            client.stop();
        }
        if let Some(udp) = self.inner.udp.lock().take() {
            udp.destroy();
        }
    }

    pub fn prepare_audio_packet_fast(
        &self,
        opus: &[u8],
        sequence: u16,
        timestamp: u32,
    ) -> bool {
        let mut log = None;
        {
            let mut guard = self.inner.shared.lock();
            let shared = &mut *guard;
            if shared.code != NetCode::Ready {
                shared.prepared.clear();
                return false;
            }

            let outcome = shared.dave.encrypt_into(opus, &mut shared.dave_frame);
            let payload: &[u8] = match outcome {
                EncryptOutcome::Encrypted => &shared.dave_frame,
                EncryptOutcome::Passthrough => opus,
                EncryptOutcome::Drop => {
                    shared.prepared.clear();
                    return false;
                }
            };

            let packet = &mut shared.prepared;
            packet.clear();
            packet.resize(12, 0);
            packet[0] = 0x80;
            packet[1] = 0x78;
            packet[2..4].copy_from_slice(&sequence.to_be_bytes());
            packet[4..8].copy_from_slice(&timestamp.to_be_bytes());
            packet[8..12].copy_from_slice(&shared.ssrc.to_be_bytes());

            let mut header = [0u8; 12];
            header.copy_from_slice(&packet[..12]);

            shared.nonce = shared.nonce.wrapping_add(1);
            let counter = shared.nonce;
            let mut nonce = [0u8; 24];
            nonce[..4].copy_from_slice(&counter.to_be_bytes());

            packet.reserve(payload.len() + 20);
            crypto::seal_append(&shared.key, &nonce, &header, payload, packet);
            packet.extend_from_slice(&nonce[..4]);

            if !shared.rtp_logged {
                shared.rtp_logged = true;
                log = Some(format!(
                    "[RTP] ssrc={} seq={} ts={} mode={} key={}",
                    shared.ssrc,
                    sequence,
                    timestamp,
                    shared.mode,
                    shared.key.len()
                ));
            }
        }

        if let Some(log) = log {
            if self.inner.debug {
                self.inner.state.debug(log);
            }
        }
        true
    }

    pub fn dispatch_audio(&self) -> bool {
        let udp = self.inner.udp.lock().clone();
        let mut log = None;
        let sent = {
            let mut shared = self.inner.shared.lock();
            if shared.code != NetCode::Ready || shared.prepared.is_empty() {
                return false;
            }

            let sent = udp
                .map(|socket| socket.send(&shared.prepared))
                .unwrap_or(false);
            if !shared.audio_send_logged {
                shared.audio_send_logged = true;
                log = Some((shared.prepared.len(), sent));
            }
            shared.prepared.clear();
            sent
        };

        if let Some((bytes, sent)) = log {
            if self.inner.debug {
                self.inner.state.debug(format!(
                    "[UDP] audio send {} bytes ok={}",
                    bytes,
                    if sent { 1 } else { 0 }
                ));
            }
        }
        sent
    }

    pub fn txtime_active(&self) -> bool {
        self.inner.shared.lock().txtime
    }

    pub fn dispatch_audio_at(&self, txtime_ns: u64) -> bool {
        let Some(udp) = self.inner.udp.lock().clone() else {
            return false;
        };
        let mut buffer = [0u8; 1500];
        let length = {
            let mut shared = self.inner.shared.lock();
            if shared.code != NetCode::Ready || shared.prepared.is_empty() {
                return false;
            }
            let length = shared.prepared.len();
            if length > buffer.len() {
                let sent = udp.send_at(&shared.prepared, txtime_ns);
                shared.prepared.clear();
                return sent;
            }
            buffer[..length].copy_from_slice(&shared.prepared);
            shared.prepared.clear();
            length
        };
        udp.send_at(&buffer[..length], txtime_ns)
    }

    pub fn txtime_errors(&self) -> (u64, i32) {
        self.inner
            .udp
            .lock()
            .as_ref()
            .map(|socket| socket.txtime_errors())
            .unwrap_or((0, 0))
    }

    pub fn set_speaking(&self, speaking: bool) {
        let ssrc = {
            let mut shared = self.inner.shared.lock();
            if shared.code != NetCode::Ready || shared.speaking == speaking {
                return;
            }
            shared.speaking = speaking;
            shared.ssrc
        };

        if let Some(conn) = self.inner.conn.lock().as_ref() {
            conn.send_json(&json!({
                "op": 5,
                "d": {
                    "speaking": if speaking { 1 } else { 0 },
                    "delay": 0,
                    "ssrc": ssrc,
                },
            }));
        }
    }

    pub fn dave_ready(&self) -> bool {
        self.inner.shared.lock().dave.ready_for_playback()
    }
}

fn handle_event(inner: &Arc<NetInner>, event: WsEvent, conn: &WsConn) {
    match event {
        WsEvent::Open => {
            {
                let mut shared = inner.shared.lock();
                if shared.code == NetCode::Closed {
                    return;
                }
                shared.code = NetCode::Identifying;
            }
            send_identify(inner, conn);
        }
        WsEvent::Json(packet) => handle_json(inner, &packet, conn),
        WsEvent::Binary { opcode, payload } => handle_binary(inner, opcode, &payload, conn),
        WsEvent::Close(code) => {
            close_connection(inner, code, None);
        }
        WsEvent::Error(message) => {
            inner.state.emit_error(message);
        }
    }
}

fn send_identify(inner: &Arc<NetInner>, conn: &WsConn) {
    conn.send_json(&json!({
        "op": 0,
        "d": {
            "server_id": inner.options.server_id,
            "user_id": inner.options.user_id,
            "session_id": inner.options.session_id,
            "token": inner.options.token,
            "max_dave_protocol_version": Dave::max_protocol_version(),
        },
    }));
}

fn handle_json(inner: &Arc<NetInner>, packet: &Value, conn: &WsConn) {
    let Some(opcode) = packet.get("op").and_then(Value::as_i64) else {
        return;
    };

    match opcode {
        8 => {
            if let Some(interval) = packet
                .get("d")
                .and_then(|data| data.get("heartbeat_interval"))
                .and_then(Value::as_u64)
            {
                conn.set_heartbeat_interval(interval);
            }
        }
        2 => handle_voice_ready(inner, packet.get("d"), conn),
        4 => handle_session_description(inner, packet.get("d"), conn),
        11 => {
            if let Some(ids) = packet
                .get("d")
                .and_then(|data| data.get("user_ids"))
                .and_then(Value::as_array)
            {
                let mut shared = inner.shared.lock();
                for id in ids {
                    if let Some(id) = id.as_str() {
                        shared.connected.insert(id.to_string());
                    }
                }
            }
        }
        13 => {
            if let Some(id) = packet
                .get("d")
                .and_then(|data| data.get("user_id"))
                .and_then(Value::as_str)
            {
                inner.shared.lock().connected.remove(id);
            }
        }
        21 => {
            let Some(data) = packet.get("d") else {
                return;
            };
            let transition_id = data
                .get("transition_id")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u16;
            let version = data
                .get("protocol_version")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u16;

            {
                let mut shared = inner.shared.lock();
                shared.dave.prepare_transition(transition_id, version);
            }
            if transition_id != 0 {
                send_transition_ready(conn, transition_id);
            }
        }
        22 => {
            if let Some(transition_id) = packet
                .get("d")
                .and_then(|data| data.get("transition_id"))
                .and_then(Value::as_u64)
            {
                inner
                    .shared
                    .lock()
                    .dave
                    .execute_transition(transition_id as u16);
            }
        }
        24 => {
            let Some(data) = packet.get("d") else {
                return;
            };
            let epoch = data.get("epoch").and_then(Value::as_u64).unwrap_or(0);
            let version = data
                .get("protocol_version")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u16;
            if epoch == 1 && version > 0 {
                initialize_dave(inner, version, conn);
            }
        }
        _ => {}
    }
}

fn handle_voice_ready(inner: &Arc<NetInner>, data: Option<&Value>, conn: &WsConn) {
    let Some(data) = data else {
        return;
    };
    {
        let shared = inner.shared.lock();
        if shared.code != NetCode::Identifying {
            return;
        }
    }

    let ip = data
        .get("ip")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let port = data.get("port").and_then(Value::as_u64).unwrap_or(0);
    let ssrc = data.get("ssrc").and_then(Value::as_u64).unwrap_or(0) as u32;
    let modes: Vec<String> = data
        .get("modes")
        .and_then(Value::as_array)
        .map(|modes| {
            modes
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    if ip.is_empty() || port == 0 {
        inner
            .state
            .emit_error("Invalid voice ready payload".to_string());
        close_connection(inner, 0, Some(conn));
        return;
    }

    let callbacks = UdpCallbacks {
        on_debug: if inner.debug {
            let state = inner.state.clone();
            Some(Arc::new(move |message: String| {
                state.debug(format!("[UDP] {}", message));
            }))
        } else {
            None
        },
        on_close: {
            let inner = inner.clone();
            Some(Arc::new(move || {
                close_connection(&inner, 0, None);
            }))
        },
    };

    let udp = match VoiceUdpSocket::open(
        SocketConfig {
            ip,
            port: port as u16,
        },
        callbacks,
    ) {
        Ok(udp) => udp,
        Err(error) => {
            inner
                .state
                .emit_error(format!("Failed to open UDP socket: {error}"));
            close_connection(inner, 0, Some(conn));
            return;
        }
    };

    let mut txtime_error: Option<String> = None;
    {
        let mut shared = inner.shared.lock();
        if shared.code == NetCode::Closed {
            drop(shared);
            udp.destroy();
            return;
        }
        shared.ssrc = ssrc;
        shared.code = NetCode::UdpHandshaking;
        if inner.options.txtime {
            match udp.enable_txtime() {
                Ok(()) => shared.txtime = true,
                Err(error) => {
                    shared.txtime = false;
                    txtime_error = Some(error.to_string());
                }
            }
        }
    }
    if let Some(error) = txtime_error {
        inner.state.emit_error(format!(
            "SO_TXTIME unavailable ({error}); falling back to normal send. Run as root (CAP_NET_ADMIN) on Linux."
        ));
    }
    *inner.udp.lock() = Some(udp.clone());

    if inner.debug {
        inner
            .state
            .debug(format!("[NW] voice ready ssrc={} port={}", ssrc, port));
    }

    let inner = inner.clone();
    let conn = conn.clone();
    thread::spawn(move || {
        let Some(local) = udp.perform_ip_discovery(ssrc) else {
            let closed = { inner.shared.lock().code == NetCode::Closed };
            if !closed {
                inner
                    .state
                    .emit_error("Voice IP discovery failed".to_string());
                close_connection(&inner, 0, Some(&conn));
            }
            return;
        };

        {
            let shared = inner.shared.lock();
            if shared.code != NetCode::UdpHandshaking {
                return;
            }
        }

        let Some(mode) = modes
            .iter()
            .find(|mode| mode.as_str() == ENCRYPTION_MODE)
            .cloned()
        else {
            inner
                .state
                .emit_error("No supported encryption mode".to_string());
            return;
        };

        conn.send_json(&json!({
            "op": 1,
            "d": {
                "protocol": "udp",
                "data": {
                    "address": local.ip,
                    "port": local.port,
                    "mode": mode,
                },
            },
        }));

        inner.shared.lock().code = NetCode::SelectingProtocol;
    });
}

fn handle_session_description(inner: &Arc<NetInner>, data: Option<&Value>, conn: &WsConn) {
    let Some(data) = data else {
        return;
    };
    {
        let shared = inner.shared.lock();
        if shared.code != NetCode::SelectingProtocol {
            return;
        }
    }

    let mode = data
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut key = [0u8; 32];
    let mut key_length = 0;
    if let Some(bytes) = data.get("secret_key").and_then(Value::as_array) {
        for byte in bytes {
            if key_length >= key.len() {
                break;
            }
            if let Some(value) = byte.as_i64() {
                key[key_length] = (value & 0xFF) as u8;
                key_length += 1;
            }
        }
    }

    if mode != ENCRYPTION_MODE {
        inner
            .state
            .emit_error(format!("Unsupported encryption mode {}", mode));
        return;
    }
    if key_length != key.len() {
        inner
            .state
            .emit_error("Invalid voice secret key".to_string());
        return;
    }

    let dave_version = data
        .get("dave_protocol_version")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u16;

    {
        let mut shared = inner.shared.lock();
        shared.mode = mode.clone();
        shared.key = key;
        shared.sequence = crypto::random_u16();
        shared.timestamp = crypto::random_u32();
        shared.nonce = 0;
        shared.speaking = false;
        shared.dave_version = dave_version;
        shared.code = NetCode::Ready;
    }

    if inner.debug {
        inner.state.debug(format!(
            "[NW] encryption {} dave={}",
            mode, dave_version
        ));
    }

    if dave_version > 0 {
        initialize_dave(inner, dave_version, conn);
    }

    inner.state.net_ready();
}

fn initialize_dave(inner: &Arc<NetInner>, version: u16, conn: &WsConn) {
    if inner.options.channel_id.is_empty() {
        inner
            .state
            .emit_error("Missing channel id for DAVE".to_string());
        return;
    }

    let channel_id = inner.options.channel_id.clone();
    let user_id = inner.options.user_id.clone();
    let mut shared = inner.shared.lock();
    shared.dave.set_epoch_version(version);
    if let Some(external_sender) = shared.external_sender.clone() {
        shared.dave.set_external_sender(&external_sender);
    }

    match shared.dave.init(version, &channel_id, &user_id) {
        Ok(key_package) => {
            shared.dave.clear_resets();
            drop(shared);
            conn.send_binary(26, &key_package);
            if inner.debug {
                inner.state.debug(format!(
                    "[DAVE] session initialized v{} key package {} bytes",
                    version,
                    key_package.len()
                ));
            }
        }
        Err(error) => {
            drop(shared);
            inner.state.emit_error(error);
        }
    }
}

fn handle_binary(inner: &Arc<NetInner>, opcode: u8, payload: &[u8], conn: &WsConn) {
    match opcode {
        25 => {
            let mut shared = inner.shared.lock();
            shared.external_sender = Some(payload.to_vec());
            shared.dave.set_external_sender(payload);
            if inner.debug {
                drop(shared);
                inner
                    .state
                    .debug(format!("[DAVE] external sender {} bytes", payload.len()));
            }
        }
        27 => {
            if payload.is_empty() {
                return;
            }
            let operation = payload[0];
            let proposals = &payload[1..];
            let recognized = recognized_user_ids(inner);

            let mut shared = inner.shared.lock();
            if !shared.dave.initialized() {
                return;
            }
            match shared.dave.process_proposals(operation, proposals, &recognized) {
                Ok(Some(combined)) => {
                    drop(shared);
                    conn.send_binary(28, &combined);
                }
                Ok(None) => {
                    drop(shared);
                    if inner.debug {
                        inner
                            .state
                            .debug("[DAVE] proposals produced no commit".to_string());
                    }
                }
                Err(error) => {
                    drop(shared);
                    inner.state.emit_error(error);
                }
            }
        }
        29 | 30 => {
            if payload.len() < 2 {
                return;
            }
            let transition_id =
                u16::from_be_bytes([payload[0], payload[1]]);
            let body = &payload[2..];
            let recognized = recognized_user_ids(inner);

            let result = {
                let mut shared = inner.shared.lock();
                if !shared.dave.initialized() {
                    return;
                }
                if opcode == 29 {
                    shared.dave.process_commit(transition_id, body)
                } else {
                    shared
                        .dave
                        .process_welcome(transition_id, body, &recognized)
                }
            };

            match result {
                ProcessResult::Success => {
                    if transition_id != 0 {
                        send_transition_ready(conn, transition_id);
                    }
                    if inner.debug {
                        inner.state.debug(format!(
                            "[DAVE] {} processed (transition {})",
                            if opcode == 29 { "commit" } else { "welcome" },
                            transition_id
                        ));
                    }
                }
                ProcessResult::Ignored => {}
                ProcessResult::Failed => recover_dave(inner, transition_id, conn),
            }
        }
        _ => {}
    }
}

fn recover_dave(inner: &Arc<NetInner>, transition_id: u16, conn: &WsConn) {
    conn.send_json(&json!({
        "op": 31,
        "d": { "transition_id": transition_id },
    }));

    let reinit = {
        let mut shared = inner.shared.lock();
        if !shared.dave.register_reset() {
            None
        } else {
            Some(shared.dave.reinit())
        }
    };

    match reinit {
        Some(Ok(Some(key_package))) => {
            conn.send_binary(26, &key_package);
            if inner.debug {
                inner
                    .state
                    .debug("[DAVE] session reinitialized".to_string());
            }
        }
        Some(Ok(None)) => {}
        Some(Err(error)) => {
            inner.state.emit_error(error);
        }
        None => {
            inner
                .state
                .emit_error("DAVE transition failed repeatedly".to_string());
        }
    }
}

fn send_transition_ready(conn: &WsConn, transition_id: u16) {
    conn.send_json(&json!({
        "op": 23,
        "d": { "transition_id": transition_id },
    }));
}

fn recognized_user_ids(inner: &Arc<NetInner>) -> Vec<String> {
    let shared = inner.shared.lock();
    let mut ids: Vec<String> = shared.connected.iter().cloned().collect();
    drop(shared);
    if !inner.options.user_id.is_empty() && !ids.iter().any(|id| id == &inner.options.user_id) {
        ids.push(inner.options.user_id.clone());
    }
    ids
}

fn close_connection(inner: &Arc<NetInner>, code: i32, conn: Option<&WsConn>) {
    {
        let mut shared = inner.shared.lock();
        if shared.code == NetCode::Closed {
            return;
        }
        shared.code = NetCode::Closed;
        shared.prepared.clear();
    }

    if code == 4017 {
        inner.state.emit_error(
            "Voice channel rejected the DAVE handshake (4017)".to_string(),
        );
    }

    if let Some(udp) = inner.udp.lock().take() {
        udp.destroy();
    }
    inner.state.net_closed(code);
    if let Some(conn) = conn {
        conn.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EncodedAudio;

    #[test]
    fn silence_frame_is_canonical() {
        assert_eq!(EncodedAudio::silence_frame(), &[0xF8, 0xFF, 0xFE]);
    }

    #[test]
    fn dave_max_version() {
        assert_eq!(Dave::max_protocol_version(), 1);
    }

    #[test]
    fn encryption_mode_supported() {
        assert_eq!(ENCRYPTION_MODE, "aead_xchacha20_poly1305_rtpsize");
    }
}
