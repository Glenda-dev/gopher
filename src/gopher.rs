use crate::GlendaNetDevice;
use crate::log;
use alloc::collections::BTreeMap;
use alloc::vec;
use glenda::cap::RECV_SLOT;
use glenda::cap::{CapPtr, Endpoint, Reply};
use glenda::client::device::net::NetClient;
use glenda::client::device::timer::TimerClient;
use glenda::error::Error;
use glenda::interface::{NetDevice, NetworkService, SystemService, TimerDevice};
use glenda::ipc::server::handle_call;
use glenda::ipc::{Badge, MsgTag, UTCB};
use glenda::protocol;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpAddress, IpCidr};

pub struct GopherManager<'a> {
    device: GlendaNetDevice,
    iface: Interface,
    sockets: SocketSet<'a>,
    timer: TimerClient,

    endpoint: Endpoint,
    reply: Reply,
    running: bool,

    // Map badges to socket handles
    socket_map: BTreeMap<Badge, SocketHandle>,
}

impl<'a> GopherManager<'a> {
    pub fn new(net_cap: Endpoint, timer_cap: Endpoint) -> Self {
        let timer = TimerClient::new(timer_cap);
        let net = NetClient::new(net_cap);
        let mac = net.mac_address();
        let mut device = GlendaNetDevice::new(net_cap);

        let config = Config::new(EthernetAddress(mac.octets).into());
        let time = Self::get_instant(&timer);
        let mut iface = Interface::new(config, &mut device, time);

        iface.update_ip_addrs(|addrs| {
            addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)).unwrap();
        });

        Self {
            device,
            iface,
            sockets: SocketSet::new(vec![]),
            timer,
            endpoint: Endpoint::from(CapPtr::null()),
            reply: Reply::from(CapPtr::null()),
            running: false,
            socket_map: BTreeMap::new(),
        }
    }

    fn get_instant(timer: &TimerClient) -> Instant {
        let time_ns = timer.get_time();
        Instant::from_micros((time_ns / 1000) as i64)
    }

    fn poll(&mut self) {
        let timestamp = Self::get_instant(&self.timer);
        self.iface.poll(timestamp, &mut self.device, &mut self.sockets);
    }

    // Helper methods to bridge Badge to SocketService
    fn bind_socket(&mut self, badge: Badge, address: &[u8]) -> Result<(), Error> {
        let handle = self.socket_map.get(&badge).ok_or(Error::NotFound)?;
        let socket = self.sockets.get_mut::<tcp::Socket>(*handle);

        if address.len() < 2 {
            return Err(Error::InvalidArgs);
        }
        let port = u16::from_le_bytes([address[0], address[1]]);

        socket.listen(port).map_err(|_| Error::InternalError)
    }

    fn listen_socket(&mut self, _badge: Badge, _backlog: i32) -> Result<(), Error> {
        Ok(())
    }

    fn send_socket(&mut self, badge: Badge, data: &[u8], _flags: i32) -> Result<usize, Error> {
        let handle = self.socket_map.get(&badge).ok_or(Error::NotFound)?;
        let socket = self.sockets.get_mut::<tcp::Socket>(*handle);

        if socket.may_send() {
            socket.send_slice(data).map_err(|_| Error::IoError)
        } else {
            Err(Error::WouldBlock)
        }
    }

    fn recv_socket(
        &mut self,
        badge: Badge,
        buffer: &mut [u8],
        _flags: i32,
    ) -> Result<usize, Error> {
        let handle = self.socket_map.get(&badge).ok_or(Error::NotFound)?;
        let socket = self.sockets.get_mut::<tcp::Socket>(*handle);

        if socket.may_recv() {
            socket.recv_slice(buffer).map_err(|_| Error::IoError)
        } else {
            Err(Error::WouldBlock)
        }
    }
}

impl<'a> SystemService for GopherManager<'a> {
    fn init(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn listen(&mut self, ep: Endpoint, reply: CapPtr) -> Result<(), Error> {
        self.endpoint = ep;
        self.reply = Reply::from(reply);
        Ok(())
    }

    fn run(&mut self) -> Result<(), Error> {
        if self.endpoint.cap().is_null() || self.reply.cap().is_null() {
            return Err(Error::NotInitialized);
        }
        self.running = true;
        while self.running {
            self.poll();

            let mut utcb = unsafe { UTCB::new() };
            utcb.clear();
            utcb.set_reply_window(self.reply.cap());
            utcb.set_recv_window(RECV_SLOT);

            match self.endpoint.recv(&mut utcb) {
                Ok(_) => {
                    if let Err(e) = self.dispatch(&mut utcb) {
                        log!("Dispatch error: {:?}", e);
                        utcb.set_msg_tag(MsgTag::err());
                        utcb.set_mr(0, e as usize);
                    }
                    self.reply(&mut utcb)?;
                }
                Err(Error::WouldBlock) | Err(Error::Timeout) => {
                    continue;
                }
                Err(e) => {
                    log!("Recv error: {:?}", e);
                    continue;
                }
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        let badge = utcb.get_badge();

        glenda::ipc_dispatch! {
            self, utcb,
            (protocol::network::PROTOCOL_ID, protocol::network::SOCKET) => |s: &mut Self, u: &mut UTCB| {
                let domain = u.get_mr(0) as i32;
                let socket_type = u.get_mr(1) as i32;
                let proto = u.get_mr(2) as i32;
                handle_call(u, |_| s.socket(domain, socket_type, proto))
            },
            (protocol::network::PROTOCOL_ID, protocol::network::BIND) => |s: &mut Self, u: &mut UTCB| {
                let res = {
                    let addr = u.buffer();
                    s.bind_socket(badge, addr)
                };
                match res {
                    Ok(_) => {
                        u.set_msg_tag(MsgTag::ok());
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            },
            (protocol::network::PROTOCOL_ID, protocol::network::LISTEN) => |s: &mut Self, u: &mut UTCB| {
                let backlog = u.get_mr(0) as i32;
                handle_call(u, |_| s.listen_socket(badge, backlog))
            },
            (protocol::network::PROTOCOL_ID, protocol::network::SEND) => |s: &mut Self, u: &mut UTCB| {
                let res = {
                    let data = u.buffer();
                    s.send_socket(badge, data, 0)
                };
                match res {
                    Ok(len) => {
                        u.set_msg_tag(MsgTag::ok());
                        u.set_mr(0, len);
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            },
            (protocol::network::PROTOCOL_ID, protocol::network::RECV) => |s: &mut Self, u: &mut UTCB| {
                let mut buf = [0u8; 2048];
                match s.recv_socket(badge, &mut buf, 0) {
                    Ok(len) => {
                        u.buffer_mut()[..len].copy_from_slice(&buf[..len]);
                        u.set_size(len);
                        u.set_msg_tag(MsgTag::ok());
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }

    fn reply(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        self.reply.reply(utcb)
    }

    fn stop(&mut self) {
        self.running = false;
    }
}

impl<'a> NetworkService for GopherManager<'a> {
    fn socket(&mut self, domain: i32, socket_type: i32, _protocol: i32) -> Result<usize, Error> {
        if domain != protocol::network::AF_INET {
            return Err(Error::InvalidArgs);
        }

        let handle = match socket_type {
            protocol::network::SOCK_STREAM => {
                let rx_buffer = tcp::SocketBuffer::new(vec![0; 4096]);
                let tx_buffer = tcp::SocketBuffer::new(vec![0; 4096]);
                let socket = tcp::Socket::new(rx_buffer, tx_buffer);
                self.sockets.add(socket)
            }
            _ => return Err(Error::NotSupported),
        };

        let badge = Badge::new(handle.as_usize());
        self.socket_map.insert(badge, handle);

        log!("Created new socket handle {}", handle);
        Ok(badge.bits())
    }
}

trait SocketHandleExt {
    fn as_usize(&self) -> usize;
}

impl SocketHandleExt for SocketHandle {
    fn as_usize(&self) -> usize {
        // Safe because smoltcp's SocketHandle is documented to be transparently a usize or similar
        // Or we can just use a separate ID generator.
        unsafe { core::mem::transmute_copy::<SocketHandle, usize>(self) }
    }
}
