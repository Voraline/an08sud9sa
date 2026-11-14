use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use socket2::{Domain, Protocol, Socket, Type};

#[cfg(target_os = "linux")]
const SO_TXTIME: libc::c_int = 61;
#[cfg(target_os = "linux")]
const SCM_TXTIME: libc::c_int = SO_TXTIME;
const DISCOVERY_SIZE: usize = 74;
const KEEPALIVE_SIZE: usize = 8;
const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(5_000);
const DISCOVERY_RESEND: Duration = Duration::from_millis(500);

#[derive(Clone, Debug)]
pub struct SocketConfig {
    pub ip: String,
    pub port: u16,
}

pub struct UdpCallbacks {
    pub on_debug: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub on_close: Option<Arc<dyn Fn() + Send + Sync>>,
}

struct Shared {
    stop: AtomicBool,
    ping_ms: AtomicI32,
    keepalive_counter: AtomicU32,
    warned: AtomicBool,
    discovery: Mutex<Option<SocketConfig>>,
    discovery_signal: Condvar,
    keepalives: Mutex<VecDeque<(u32, Instant)>>,
    callbacks: UdpCallbacks,
}

pub struct VoiceUdpSocket {
    socket: Arc<UdpSocket>,
    shared: Arc<Shared>,
    receive_thread: Mutex<Option<JoinHandle<()>>>,
    keepalive_thread: Mutex<Option<JoinHandle<()>>>,
    txtime: AtomicBool,
}

impl VoiceUdpSocket {
    pub fn open(remote: SocketConfig, callbacks: UdpCallbacks) -> std::io::Result<Arc<Self>> {
        let address: SocketAddrV4 = SocketAddrV4::new(
            remote.ip.parse::<Ipv4Addr>().map_err(|_| {
                std::io::Error::new(ErrorKind::InvalidInput, "invalid voice server address")
            })?,
            remote.port,
        );

        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        let _ = socket.set_send_buffer_size(1 << 20);
        let _ = socket.set_recv_buffer_size(1 << 20);
        socket.connect(&SocketAddr::V4(address).into())?;
        socket.set_read_timeout(Some(Duration::from_millis(250)))?;
        let socket: UdpSocket = socket.into();
        let socket = Arc::new(socket);

        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            ping_ms: AtomicI32::new(-1),
            keepalive_counter: AtomicU32::new(0),
            warned: AtomicBool::new(false),
            discovery: Mutex::new(None),
            discovery_signal: Condvar::new(),
            keepalives: Mutex::new(VecDeque::new()),
            callbacks,
        });

        let this = Arc::new(Self {
            socket: socket.clone(),
            shared: shared.clone(),
            receive_thread: Mutex::new(None),
            keepalive_thread: Mutex::new(None),
            txtime: AtomicBool::new(false),
        });

        let receive_shared = shared.clone();
        let receive_socket = socket.clone();
        *this.receive_thread.lock() = Some(thread::spawn(move || {
            receive_loop(&receive_socket, &receive_shared);
        }));

        let keepalive_shared = shared.clone();
        let keepalive_socket = socket.clone();
        *this.keepalive_thread.lock() = Some(thread::spawn(move || {
            keepalive_loop(&keepalive_socket, &keepalive_shared);
        }));

        Ok(this)
    }

    pub fn send(&self, data: &[u8]) -> bool {
        self.socket.send(data).is_ok()
    }

    pub fn txtime_active(&self) -> bool {
        self.txtime.load(Ordering::Relaxed)
    }

    #[cfg(target_os = "linux")]
    pub fn enable_txtime(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;

        #[repr(C)]
        struct SockTxtime {
            clockid: i32,
            flags: u32,
        }

        let config = SockTxtime {
            clockid: libc::CLOCK_MONOTONIC,
            flags: 0,
        };
        let result = unsafe {
            libc::setsockopt(
                self.socket.as_raw_fd(),
                libc::SOL_SOCKET,
                SO_TXTIME,
                &config as *const SockTxtime as *const libc::c_void,
                std::mem::size_of::<SockTxtime>() as libc::socklen_t,
            )
        };
        if result != 0 {
            self.txtime.store(false, Ordering::Relaxed);
            return Err(std::io::Error::last_os_error());
        }
        self.txtime.store(true, Ordering::Relaxed);
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn enable_txtime(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "SO_TXTIME is only available on Linux",
        ))
    }

    #[cfg(target_os = "linux")]
    pub fn send_at(&self, data: &[u8], txtime_ns: u64) -> bool {
        use std::os::fd::AsRawFd;

        if !self.txtime.load(Ordering::Relaxed) {
            return self.send(data);
        }

        let mut control = [0u64; 4];
        let mut iov = libc::iovec {
            iov_base: data.as_ptr() as *mut libc::c_void,
            iov_len: data.len(),
        };
        unsafe {
            let mut message: libc::msghdr = std::mem::zeroed();
            message.msg_iov = &mut iov;
            message.msg_iovlen = 1;
            message.msg_control = control.as_mut_ptr() as *mut libc::c_void;
            message.msg_controllen = libc::CMSG_SPACE(8) as _;

            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = SCM_TXTIME;
            (*header).cmsg_len = libc::CMSG_LEN(8) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(header) as *mut u64, txtime_ns);

            libc::sendmsg(self.socket.as_raw_fd(), &message, 0) >= 0
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn send_at(&self, data: &[u8], _txtime_ns: u64) -> bool {
        self.send(data)
    }

    pub fn perform_ip_discovery(&self, ssrc: u32) -> Option<SocketConfig> {
        self.shared.discovery.lock().take();

        let mut packet = [0u8; DISCOVERY_SIZE];
        packet[0..2].copy_from_slice(&1u16.to_be_bytes());
        packet[2..4].copy_from_slice(&70u16.to_be_bytes());
        packet[4..8].copy_from_slice(&ssrc.to_be_bytes());

        let deadline = Instant::now() + DISCOVERY_TIMEOUT;
        let mut guard = self.shared.discovery.lock();
        loop {
            if self.shared.stop.load(Ordering::Relaxed) {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }

            if !self.send(&packet) {
                return None;
            }

            let wait = DISCOVERY_RESEND.min(deadline.saturating_duration_since(now));
            let _ = self
                .shared
                .discovery_signal
                .wait_for(&mut guard, wait);
            if guard.is_some() {
                return guard.take();
            }
        }
    }

    pub fn ping(&self) -> Option<i32> {
        let value = self.shared.ping_ms.load(Ordering::Relaxed);
        (value >= 0).then_some(value)
    }

    pub fn destroy(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.discovery_signal.notify_all();
        if let Some(handle) = self.receive_thread.lock().take() {
            if handle.thread().id() != thread::current().id() {
                let _ = handle.join();
            }
        }
        if let Some(handle) = self.keepalive_thread.lock().take() {
            if handle.thread().id() != thread::current().id() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for VoiceUdpSocket {
    fn drop(&mut self) {
        self.destroy();
    }
}

fn receive_loop(socket: &UdpSocket, shared: &Arc<Shared>) {
    let mut buffer = [0u8; 2048];

    while !shared.stop.load(Ordering::Relaxed) {
        let received = match socket.recv(&mut buffer) {
            Ok(size) => size,
            Err(error) => {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) {
                    continue;
                }
                if let Some(callback) = &shared.callbacks.on_close {
                    callback();
                }
                return;
            }
        };
        if received == 0 {
            continue;
        }
        handle_datagram(&buffer[..received], shared);
    }
}

fn handle_datagram(data: &[u8], shared: &Arc<Shared>) {
    if data.len() == KEEPALIVE_SIZE {
        let counter = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let matched = {
            let mut keepalives = shared.keepalives.lock();
            if let Some(index) = keepalives.iter().position(|entry| entry.0 == counter) {
                let ping = keepalives[index].1.elapsed().as_millis() as i32;
                shared.ping_ms.store(ping, Ordering::Relaxed);
                keepalives.drain(..=index);
                Some(ping)
            } else {
                None
            }
        };

        if let (Some(ping), Some(callback)) = (matched, &shared.callbacks.on_debug) {
            callback(format!(
                "UDP keepalive ack counter {} ping {}",
                counter, ping
            ));
        }
        return;
    }

    if data.len() >= DISCOVERY_SIZE && u16::from_be_bytes([data[0], data[1]]) == 2 {
        let start = &data[8..];
        let end = start.iter().position(|byte| *byte == 0);
        if let Some(end) = end {
            if let Ok(ip) = std::str::from_utf8(&start[..end]) {
                if ip.parse::<Ipv4Addr>().is_ok() {
                    let port = u16::from_be_bytes([data[data.len() - 2], data[data.len() - 1]]);
                    *shared.discovery.lock() = Some(SocketConfig {
                        ip: ip.to_string(),
                        port,
                    });
                    shared.discovery_signal.notify_all();
                }
            }
        }
    }
}

fn keepalive_loop(socket: &UdpSocket, shared: &Arc<Shared>) {
    while !shared.stop.load(Ordering::Relaxed) {
        for _ in 0..50 {
            if shared.stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }

        let counter = shared.keepalive_counter.fetch_add(1, Ordering::Relaxed);
        {
            let mut keepalives = shared.keepalives.lock();
            if keepalives.len() >= 64 {
                keepalives.clear();
                if !shared.warned.swap(true, Ordering::Relaxed) {
                    if let Some(callback) = &shared.callbacks.on_debug {
                        callback("UDP keepalive responses not seen; continuing".to_string());
                    }
                }
            }
            keepalives.push_back((counter, Instant::now()));
        }

        let mut packet = [0u8; KEEPALIVE_SIZE];
        packet[..4].copy_from_slice(&counter.to_le_bytes());
        if socket.send(&packet).is_err() {
            return;
        }
    }
}
