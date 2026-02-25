use super::GopherServer;
use super::network::GopherSocket;
use crate::layout::CONFIG_SLOT;
use glenda::cap::{CapPtr, Endpoint, Reply};
use glenda::error::Error;
use glenda::interface::device::DeviceService;
use glenda::interface::resource::ResourceService;
use glenda::interface::{InitService, MemoryService, NetworkService, SocketService, SystemService};
use glenda::ipc::server::{handle_call, handle_notify};
use glenda::ipc::{Badge, MsgFlags, MsgTag, UTCB};
use glenda::protocol;
use glenda::protocol::device::{HookTarget, LogicDeviceType};
use glenda::protocol::init::ServiceState;
use glenda::utils::align::align_up;
use glenda::utils::manager::CSpaceService;

impl<'a> SystemService for GopherServer<'a> {
    fn init(&mut self) -> Result<(), Error> {
        // 0. Load Network Config (network.json)
        log!("Loading network.json...");
        match self.res_client.get_config(Badge::null(), "network.json", CONFIG_SLOT) {
            Ok((frame, size)) => {
                let size_aligned = align_up(size, 4096);
                let addr = self
                    .next_ring_vaddr
                    .fetch_add(size_aligned, core::sync::atomic::Ordering::SeqCst);
                MemoryService::mmap(self.res_client, Badge::null(), frame, addr, size_aligned)?;
                let data = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
                if let Ok(config_str) = core::str::from_utf8(data) {
                    // Truncate at first null byte if any
                    let config_str = config_str.split('\0').next().unwrap_or(config_str);
                    match serde_json::from_str::<super::NetworkConfig>(config_str) {
                        Ok(config) => {
                            log!("Network config loaded: buffer_size={}", config.buffer_size);
                            self.config = Some(config);
                        }
                        Err(e) => log!("Failed to parse network.json: {:?}", e),
                    }
                }
                // Cleanup config mapping/frame if needed? (fossil just deletes the slot)
                let _ = self.cspace.root().delete(CONFIG_SLOT);
            }
            Err(_) => {
                log!("No network.json found or failed to load");
                let _ = self.cspace.root().delete(CONFIG_SLOT);
            }
        }

        // 1. Setup global SHM for network packets
        let shm_size = self.config.as_ref().map(|c| c.buffer_size).unwrap_or(1024 * 1024);
        let shm_pages = (shm_size + 4095) / 4096;
        let shm_size_aligned = shm_pages * 4096;

        let shm_slot = self.cspace.alloc(self.res_client)?;
        let (shm_paddr, shm_frame) =
            self.res_client.dma_alloc(Badge::null(), shm_pages, shm_slot)?;
        let shm_vaddr =
            self.next_shm_vaddr.fetch_add(shm_size_aligned, core::sync::atomic::Ordering::SeqCst);
        MemoryService::mmap(
            self.res_client,
            Badge::null(),
            shm_frame.clone(),
            shm_vaddr,
            shm_size_aligned,
        )?;
        self.shm_frame = Some((shm_frame, shm_vaddr, shm_size_aligned, shm_paddr as usize));

        // 2. Setup Loopback
        self.setup_loopback();

        // 3. Register hook for future net devices
        log!("Hooked to Unicorn for network devices");
        let target = HookTarget::Type(LogicDeviceType::Net);
        self.device_client.hook(Badge::null(), target, self.endpoint.cap())?;

        // 4. Register Network service
        log!("Registering Network Service...");
        self.res_client
            .register_cap(
                Badge::null(),
                glenda::protocol::resource::ResourceType::Endpoint,
                glenda::protocol::resource::NET_ENDPOINT,
                self.endpoint.cap(),
            )
            .ok();

        // 4. Initial probe for already existing devices
        if let Ok(_) = self.sync_devices() {
            if let Err(e) = self.process_pending_probes() {
                log!("Initial probe failed: {:?}, non-critical", e);
            }
        }

        Ok(())
    }

    fn listen(&mut self, ep: Endpoint, reply: CapPtr, recv: CapPtr) -> Result<(), Error> {
        self.endpoint = ep;
        self.reply = Reply::from(reply);
        self.recv = recv;
        Ok(())
    }

    fn run(&mut self) -> Result<(), Error> {
        self.init_client.report_service(Badge::null(), ServiceState::Running)?;
        self.running = true;

        while self.running {
            // Process any pending device probes or stack maintenance
            if let Err(e) = self.process_pending_probes() {
                error!("Pending probe error: {:?}", e);
            }
            if let Err(e) = self.poll() {
                error!("Poll error: {:?}", e);
            }

            // Network stack poll
            let mut utcb = unsafe { UTCB::new() };
            utcb.clear();
            utcb.set_reply_window(self.reply.cap());
            utcb.set_recv_window(self.recv);

            if let Err(e) = self.endpoint.recv(&mut utcb) {
                error!("Recv error: {:?}", e);
                continue;
            }

            match self.dispatch(&mut utcb) {
                Ok(()) => {
                    let _ = self.reply(&mut utcb);
                }
                Err(Error::Success) | Err(Error::WouldBlock) | Err(Error::Timeout) => {
                    // Handled notification, skip reply
                }
                Err(e) => {
                    let badge = utcb.get_badge();
                    let tag = utcb.get_msg_tag();
                    log!(
                        "Dispatch error: {:?} badge={}, proto={:#x}, label={:#x}",
                        e,
                        badge,
                        tag.proto(),
                        tag.label()
                    );
                    utcb.set_msg_tag(MsgTag::err());
                    utcb.set_mr(0, e as usize);
                    let _ = self.reply(&mut utcb);
                }
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        let badge = utcb.get_badge();

        glenda::ipc_dispatch! {
            self, utcb,
            (protocol::NETWORK_PROTO, protocol::network::SOCKET) => |s: &mut Self, u: &mut UTCB| {
                let domain = u.get_mr(0) as i32;
                let socket_type = u.get_mr(1) as i32;
                let proto = u.get_mr(2) as i32;
                handle_call(u, |_| s.socket(domain, socket_type, proto))
            },
            (protocol::NETWORK_PROTO, protocol::network::BIND) => |s: &mut Self, u: &mut UTCB| {
                let res = {
                    let addr = u.buffer();
                    let mut socket = GopherSocket { server: s, badge };
                    socket.bind(addr)
                };
                match res {
                    Ok(_) => {
                        u.set_msg_tag(MsgTag::ok());
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            },
            (protocol::NETWORK_PROTO, protocol::network::LISTEN) => |s: &mut Self, u: &mut UTCB| {
                let backlog = u.get_mr(0) as i32;
                handle_call(u, |_| {
                    let mut socket = GopherSocket { server: s, badge };
                    socket.listen(backlog)
                })
            },
            (protocol::NETWORK_PROTO, protocol::network::CONNECT) => |s: &mut Self, u: &mut UTCB| {
                let res = {
                    let addr = u.buffer();
                    let mut socket = GopherSocket { server: s, badge };
                    socket.connect(addr)
                };
                match res {
                    Ok(_) => {
                        u.set_msg_tag(MsgTag::ok());
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            },
            (protocol::NETWORK_PROTO, protocol::network::ACCEPT) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |_| {
                    let mut socket = GopherSocket { server: s, badge };
                    socket.accept()
                })
            },
            (protocol::NETWORK_PROTO, protocol::network::CLOSE) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |_| {
                    let mut socket = GopherSocket { server: s, badge };
                    socket.close()
                })
            },
            (protocol::NETWORK_PROTO, protocol::network::SEND) => |s: &mut Self, u: &mut UTCB| {
                let res = {
                    let data = u.buffer();
                    let mut socket = GopherSocket { server: s, badge };
                    socket.send(data, 0)
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
            (protocol::NETWORK_PROTO, protocol::network::RECV) => |s: &mut Self, u: &mut UTCB| {
                let mut buf = [0u8; 2048];
                let mut socket = GopherSocket { server: s, badge };
                match socket.recv(&mut buf, 0) {
                    Ok(len) => {
                        u.buffer_mut()[..len].copy_from_slice(&buf[..len]);
                        u.set_size(len);
                        u.set_msg_tag(MsgTag::ok());
                        Ok(())
                    }
                    Err(e) => Err(e),
                }
            },
            (protocol::NETWORK_PROTO, protocol::network::SETUP_IOURING) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |u_inner| {
                    let addr_user = u_inner.get_mr(0);
                    let size = u_inner.get_mr(1);

                    let frame = if u_inner.get_msg_tag().flags().contains(MsgFlags::HAS_CAP) {
                        let slot = s.cspace.alloc(s.res_client)?;
                        glenda::cap::CSPACE_CAP.move_cap(glenda::cap::RECV_SLOT, slot)?;
                        Some(glenda::cap::Frame::from(slot))
                    } else {
                        None
                    };

                    let mut socket = GopherSocket { server: s, badge };
                    socket.setup_iouring(addr_user, size, frame)
                })
            },
            (protocol::NETWORK_PROTO, protocol::network::PROCESS_IOURING) => |s: &mut Self, u: &mut UTCB| {
                handle_call(u, |_| {
                    let mut socket = GopherSocket { server: s, badge };
                    socket.process_iouring()
                })
            },
            (glenda::protocol::KERNEL_PROTO, glenda::protocol::kernel::NOTIFY) => |s: &mut Self, u: &mut UTCB| {
                handle_notify(u, |u| {
                    let badge = u.get_badge();
                    let bits = badge.bits();

                    // Determine flags
                    let is_cq = bits & glenda::io::uring::NOTIFY_IO_URING_CQ != 0;
                    let is_sq = bits & glenda::io::uring::NOTIFY_IO_URING_SQ != 0;
                    let is_hook = bits & glenda::protocol::device::NOTIFY_HOOK != 0;

                    // 1. Check for device synchronization notifications
                    if is_hook {
                        if let Err(e) = s.handle_notify_sync() {
                            error!("Sync failed: {:?}", e);
                        }
                    }
                    if is_sq || is_cq {
                        if let Err(e) = s.poll() {
                            error!("Poll failed: {:?}", e);
                        }
                    }
                    Ok(())
                })?;
                Err(Error::Success)
            },
        }
    }

    fn reply(&mut self, utcb: &mut UTCB) -> Result<(), Error> {
        self.reply.reply(utcb)
    }

    fn stop(&mut self) {
        self.running = false;
    }
}

impl<'a> GopherServer<'a> {
    pub fn poll(&mut self) -> Result<(), Error> {
        let timestamp = smoltcp::time::Instant::from_micros(0); // Placeholder timer
        for ctx in &mut self.interfaces {
            let r = ctx.iface.poll(timestamp, &mut ctx.device, &mut self.sockets);
            if r == smoltcp::iface::PollResult::SocketStateChanged {
                log!("Socket state changed");
            }
        }
        Ok(())
    }
}
