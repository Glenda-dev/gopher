use super::GopherServer;
use glenda::cap::Frame;
use glenda::error::Error;
use glenda::interface::{MemoryService, NetworkService, SocketService};
use glenda::io::uring::{IOURING_OP_READ, IOURING_OP_WRITE};
use glenda::ipc::Badge;
use glenda::protocol;
use glenda::utils::align::align_up;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp;

pub struct GopherSocket<'a, 'b> {
    pub server: &'a mut GopherServer<'b>,
    pub badge: Badge,
}

impl<'a, 'b> NetworkService for GopherServer<'a> {
    fn socket(&mut self, domain: i32, socket_type: i32, _protocol: i32) -> Result<usize, Error> {
        if domain != protocol::network::AF_INET {
            return Err(Error::InvalidArgs);
        }

        let handle = match socket_type {
            protocol::network::SOCK_STREAM => {
                let rx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 4096]);
                let tx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 4096]);
                let socket = tcp::Socket::new(rx_buffer, tx_buffer);
                self.sockets.add(socket)
            }
            _ => return Err(Error::NotSupported),
        };

        let id = unsafe { core::mem::transmute_copy::<SocketHandle, usize>(&handle) };
        let badge = Badge::new(id);
        self.socket_map.insert(badge, handle);

        Ok(badge.bits())
    }
}

impl<'a, 'b> SocketService for GopherSocket<'a, 'b> {
    fn bind(&mut self, _address: &[u8]) -> Result<(), Error> {
        let _handle = self.server.socket_map.get(&self.badge).ok_or(Error::NotFound)?;
        // For now, smoltcp handles this differently or it's a stub
        Ok(())
    }

    fn listen(&mut self, _backlog: i32) -> Result<(), Error> {
        let _handle = self.server.socket_map.get(&self.badge).ok_or(Error::NotFound)?;
        // Implementation logic ...
        Ok(())
    }

    fn accept(&mut self) -> Result<usize, Error> {
        log!("Accept stub called");
        Err(Error::NotSupported)
    }

    fn connect(&mut self, _address: &[u8]) -> Result<(), Error> {
        log!("Connect stub called");
        Err(Error::NotSupported)
    }

    fn send(&mut self, data: &[u8], _flags: i32) -> Result<usize, Error> {
        let handle = self.server.socket_map.get(&self.badge).ok_or(Error::NotFound)?;
        let socket = self.server.sockets.get_mut::<tcp::Socket>(*handle);
        if !socket.can_send() {
            return Err(Error::WouldBlock);
        }
        socket.send_slice(data).map_err(|_| Error::Generic)
    }

    fn recv(&mut self, buffer: &mut [u8], _flags: i32) -> Result<usize, Error> {
        let handle = self.server.socket_map.get(&self.badge).ok_or(Error::NotFound)?;
        let socket = self.server.sockets.get_mut::<tcp::Socket>(*handle);
        if !socket.can_recv() {
            return Err(Error::WouldBlock);
        }
        socket.recv_slice(buffer).map_err(|_| Error::Generic)
    }

    fn close(&mut self) -> Result<(), Error> {
        log!("Close socket for badge {}", self.badge.bits());
        self.server.socket_map.remove(&self.badge);
        Ok(())
    }

    fn get_sockname(&self, _address: &mut [u8]) -> Result<usize, Error> {
        Err(Error::NotSupported)
    }

    fn get_peername(&self, _address: &mut [u8]) -> Result<usize, Error> {
        Err(Error::NotSupported)
    }

    fn setsockopt(&mut self, _level: i32, _optname: i32, _optval: &[u8]) -> Result<(), Error> {
        Err(Error::NotSupported)
    }

    fn getsockopt(&self, _level: i32, _optname: i32, _optval: &mut [u8]) -> Result<usize, Error> {
        Err(Error::NotSupported)
    }

    fn setup_iouring(
        &mut self,
        _client_vaddr: usize,
        size: usize,
        frame: Option<Frame>,
    ) -> Result<(), Error> {
        let _handle = self.server.socket_map.get(&self.badge).ok_or(Error::NotFound)?;
        let size_aligned = align_up(size, 4096);
        // In GopherServer, we allocate the server vaddr

        let addr_server = self
            .server
            .next_ring_vaddr
            .fetch_add(size_aligned, core::sync::atomic::Ordering::SeqCst);
        if let Some(f) = frame {
            self.server.res_client.mmap(Badge::null(), f, addr_server, size_aligned)?;
        }

        let ring =
            unsafe { glenda::io::uring::IoUringBuffer::attach(addr_server as *mut u8, size) };
        let uring_server = glenda::io::uring::IoUringServer::new(ring);
        self.server.uring_servers.insert(self.badge, uring_server);
        Ok(())
    }

    fn process_iouring(&mut self) -> Result<(), Error> {
        let mut uring_server =
            self.server.uring_servers.remove(&self.badge).ok_or(Error::NotFound)?;

        while let Some(sqe) = uring_server.next_request() {
            match sqe.opcode {
                IOURING_OP_READ => {
                    let buf = unsafe {
                        core::slice::from_raw_parts_mut(sqe.addr as *mut u8, sqe.len as usize)
                    };
                    match self.recv(buf, 0) {
                        Ok(len) => {
                            let _ = uring_server.complete(sqe.user_data, len as i32);
                        }
                        Err(e) => {
                            let _ = uring_server.complete(sqe.user_data, -(e as i32));
                        }
                    }
                }
                IOURING_OP_WRITE => {
                    let buf = unsafe {
                        core::slice::from_raw_parts(sqe.addr as *const u8, sqe.len as usize)
                    };
                    match self.send(buf, 0) {
                        Ok(len) => {
                            let _ = uring_server.complete(sqe.user_data, len as i32);
                        }
                        Err(e) => {
                            let _ = uring_server.complete(sqe.user_data, -(e as i32));
                        }
                    }
                }
                _ => {
                    let _ = uring_server.complete(sqe.user_data, -(Error::NotSupported as i32));
                }
            }
        }

        self.server.uring_servers.insert(self.badge, uring_server);
        Ok(())
    }
}
