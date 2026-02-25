use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use glenda::cap::{CapPtr, Endpoint, Reply};
use glenda::client::{DeviceClient, InitClient, ProcessClient, ResourceClient};
use glenda::error::Error;
use glenda::interface::device::DeviceService;
use glenda::interface::memory::MemoryService;
use glenda::interface::resource::ResourceService;
use glenda::io::uring::IoUringServer;
use glenda::ipc::Badge;
use glenda::utils::manager::{CSpaceManager, CSpaceService};
use glenda_drivers::interface::NetDriver;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

pub mod network;
pub mod server;
pub mod stack;

use crate::device::GlendaNetDevice;
use crate::layout::{RING_VA, SHM_VA};
use stack::{DeviceVariant, InterfaceContext};

pub struct GopherServer<'a> {
    pub res_client: &'a mut ResourceClient,
    pub _process_client: &'a mut ProcessClient,
    pub cspace: &'a mut CSpaceManager,
    pub device_client: &'a mut DeviceClient,
    pub init_client: &'a mut InitClient,

    pub endpoint: Endpoint,
    pub reply: Reply,
    pub recv: CapPtr,
    pub running: bool,

    pub interfaces: Vec<InterfaceContext>,
    pub sockets: SocketSet<'a>,
    pub socket_map: BTreeMap<Badge, SocketHandle>,
    pub uring_servers: BTreeMap<Badge, IoUringServer>,

    pub next_ring_vaddr: AtomicUsize,
    pub next_shm_vaddr: AtomicUsize,

    pub _pending_devices: Vec<String>,
}

impl<'a> GopherServer<'a> {
    pub fn new(
        endpoint: Endpoint,
        res_client: &'a mut ResourceClient,
        process_client: &'a mut ProcessClient,
        cspace: &'a mut CSpaceManager,
        device_client: &'a mut DeviceClient,
        init_client: &'a mut InitClient,
    ) -> Self {
        Self {
            res_client,
            _process_client: process_client,
            cspace,
            device_client,
            init_client,
            endpoint,
            reply: Reply::from(CapPtr::null()),
            recv: CapPtr::null(),
            running: false,
            interfaces: Vec::new(),
            sockets: SocketSet::new(Vec::new()),
            socket_map: BTreeMap::new(),
            uring_servers: BTreeMap::new(),
            next_ring_vaddr: AtomicUsize::new(RING_VA),
            next_shm_vaddr: AtomicUsize::new(SHM_VA),
            _pending_devices: Vec::new(),
        }
    }

    pub fn setup_loopback(&mut self) {
        let mut loopback_device =
            DeviceVariant::Loopback(smoltcp::phy::Loopback::new(smoltcp::phy::Medium::Ethernet));
        let loopback_config =
            Config::new(HardwareAddress::Ethernet(EthernetAddress([0, 0, 0, 0, 0, 0])));
        let time = smoltcp::time::Instant::from_micros(0);
        let mut loopback_iface = Interface::new(loopback_config, &mut loopback_device, time);
        loopback_iface.update_ip_addrs(|addrs| {
            addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).unwrap();
        });
        self.interfaces.push(InterfaceContext { device: loopback_device, iface: loopback_iface });
    }

    pub fn probe_device(&mut self, name: &str) -> Result<(), Error> {
        log!("Gopher: Probing network device {}...", name);
        let slot = self.cspace.alloc(self.res_client)?;

        self.device_client.alloc_logic(Badge::null(), 3, name, slot)?;
        let mut net_device = GlendaNetDevice::new(Endpoint::from(slot), name);

        // Setup io_uring
        let ring_slot = self.cspace.alloc(self.res_client)?;
        let ring_va = self.next_ring_vaddr.fetch_add(0x10000, Ordering::SeqCst);
        let (_, ring_frame) = self.res_client.dma_alloc(Badge::null(), 1, ring_slot)?;
        self.res_client.mmap(Badge::null(), ring_frame, ring_va, 4096)?;

        let ring_buffer =
            unsafe { glenda::io::uring::IoUringBuffer::new(ring_va as *mut u8, 4096, 4, 4) };
        let mut uring_client = glenda::io::uring::IoUringClient::new(ring_buffer);
        uring_client.set_server_notify(Endpoint::from(slot));

        // Create notification endpoint for driver to notify us
        let notify_slot = self.cspace.alloc(self.res_client)?;
        let notify_badge =
            Badge::new(glenda::io::uring::NOTIFY_IO_URING_CQ | (self.interfaces.len() << 8)); // Use bits to identify interface
        self.cspace.root().mint(
            self.endpoint.cap(),
            notify_slot,
            notify_badge,
            glenda::cap::Rights::ALL,
        )?;

        net_device.setup_ring(4, 4, Endpoint::from(notify_slot), ring_slot)?;
        net_device.client.set_ring(uring_client);

        // Setup SHM
        let shm_slot = self.cspace.alloc(self.res_client)?;
        let shm_va = self.next_shm_vaddr.fetch_add(0x40000, Ordering::SeqCst);
        let (shm_paddr, shm_frame) = self.res_client.dma_alloc(Badge::null(), 4, shm_slot)?;
        self.res_client.mmap(Badge::null(), shm_frame, shm_va, 4 * 4096)?;

        net_device.setup_shm(shm_frame, shm_va, shm_paddr as u64, 4 * 4096)?;

        // Add to stack
        let mut device_variant = DeviceVariant::Net(net_device);
        let mac = match &device_variant {
            DeviceVariant::Net(d) => d.mac_address(),
            _ => unreachable!(),
        };
        let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac.octets)));
        let time = smoltcp::time::Instant::from_micros(0);
        let mut iface = Interface::new(config, &mut device_variant, time);

        iface.update_ip_addrs(|addrs| {
            let last = mac.octets[5];
            addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15 + last as u8), 24)).unwrap();
        });

        self.interfaces.push(InterfaceContext { device: device_variant, iface });
        log!("Gopher: Device {} initialized with MAC {:?}", name, mac);

        Ok(())
    }

    pub fn process_pending_probes(&mut self) -> Result<(), Error> {
        // Query for new devices
        let query = glenda::protocol::device::DeviceQuery {
            name: None,
            compatible: Vec::new(),
            dev_type: Some(3), // 3 = Net
        };
        let devices = self.device_client.query(Badge::null(), query)?;

        for name in devices {
            // Check if already probed
            let mut _found = false;
            for name_prefix in &["lo"] {
                if name == *name_prefix {
                    _found = true;
                    break;
                }
            }
            // For now just check against interfaces
            // In a real system, we might need a better way to track "already probed" names

            // Simple check: if we initialized it, it should be in interfaces (except lo)
            // But probe_device is called by main initially too.
            // Let's just try to probe and if it's already used, Unicorn will return error or we can check.

            // For now, let's just assume we only probe once per device name.
            // This is a bit naive but works for now.
            let already_exists = self.interfaces.iter().any(|ctx| match &ctx.device {
                DeviceVariant::Net(d) => d.name() == name,
                _ => false,
            });

            if !already_exists {
                self.probe_device(&name)?;
            }
        }
        Ok(())
    }
}
