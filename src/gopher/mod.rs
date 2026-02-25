use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use glenda::cap::{CapPtr, Endpoint, Reply, Rights};
use glenda::client::{DeviceClient, InitClient, ProcessClient, ResourceClient};
use glenda::error::Error;
use glenda::interface::device::DeviceService;
use glenda::interface::memory::MemoryService;
use glenda::interface::resource::ResourceService;
use glenda::io::uring::IoUringServer;
use glenda::ipc::Badge;
use glenda::protocol::device::LogicDeviceType;
use glenda::utils::manager::{CSpaceManager, CSpaceService};
use glenda_drivers::interface::NetDriver;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address};

pub mod network;
pub mod server;
pub mod stack;

use crate::device::GlendaNetDevice;
use crate::layout::{RING_VA, SHM_VA};
use stack::{DeviceVariant, InterfaceContext};

const PGSIZE: usize = 4096;

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

    pub pending_devices: VecDeque<String>,
    pub probed_hardware: BTreeSet<u64>,
}

impl<'a> GopherServer<'a> {
    pub fn new(
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
            endpoint: Endpoint::from(CapPtr::null()),
            reply: Reply::from(CapPtr::null()),
            recv: CapPtr::null(),
            running: false,
            interfaces: Vec::new(),
            sockets: SocketSet::new(Vec::new()),
            socket_map: BTreeMap::new(),
            uring_servers: BTreeMap::new(),
            next_ring_vaddr: AtomicUsize::new(RING_VA),
            next_shm_vaddr: AtomicUsize::new(SHM_VA),
            pending_devices: VecDeque::new(),
            probed_hardware: BTreeSet::new(),
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

    pub fn sync_devices(&mut self) -> Result<(), Error> {
        log!("Syncing network devices from Unicorn...");
        let query = glenda::protocol::device::DeviceQuery {
            name: None,
            compatible: alloc::vec![],
            dev_type: Some(2), // Type 2 = Net
        };
        let names = self.device_client.query(Badge::null(), query)?;
        log!("Result: {:?}", names);
        for name in names {
            if !self.pending_devices.contains(&name) {
                self.pending_devices.push_back(name);
            }
        }
        Ok(())
    }

    pub fn process_pending_probes(&mut self) -> Result<(), Error> {
        while let Some(name) = self.pending_devices.pop_front() {
            let (hw_id, desc) = self.device_client.get_logic_desc(Badge::null(), &name)?;
            if let LogicDeviceType::Net = desc.dev_type {
                if !self.probed_hardware.contains(&hw_id) {
                    let hw_slot = self.cspace.alloc(self.res_client)?;
                    let hw_ep = self.device_client.alloc_logic(Badge::null(), 2, &name, hw_slot)?;
                    self.probe(hw_id, desc, hw_ep)?;
                }
            }
        }
        Ok(())
    }

    pub fn handle_notify_sync(&mut self) -> Result<(), Error> {
        self.sync_devices()
    }

    pub fn probe(
        &mut self,
        hw_id: u64,
        desc: glenda::protocol::device::LogicDeviceDesc,
        hardware_ep: Endpoint,
    ) -> Result<(), Error> {
        log!("Probing network device {} (hw_id={:x})", desc.name, hw_id);

        let mut net_device = GlendaNetDevice::new(hardware_ep, &desc.name);

        // Setup notification endpoint for driver to signal us
        let notify_slot = self.cspace.alloc(self.res_client)?;
        self.cspace.root().mint(
            self.endpoint.cap(),
            notify_slot,
            Badge::new(hw_id as usize),
            Rights::ALL,
        )?;
        let notify_ep = Endpoint::from(notify_slot);

        // Setup IO ring
        let ring_va = self.next_ring_vaddr.fetch_add(PGSIZE, Ordering::SeqCst);
        let ring_frame = net_device.setup_ring(4, 4, notify_ep, self.recv)?;
        self.res_client.mmap(Badge::null(), ring_frame, ring_va, PGSIZE)?;

        // Setup SHM
        let shm_slot = self.cspace.alloc(self.res_client)?;
        let shm_va = self.next_shm_vaddr.fetch_add(4 * PGSIZE, Ordering::SeqCst);
        let (shm_paddr, shm_frame) = self.res_client.dma_alloc(Badge::null(), 4, shm_slot)?;
        self.res_client.mmap(Badge::null(), shm_frame.clone(), shm_va, 4 * PGSIZE)?;
        net_device.setup_shm(shm_frame, shm_va, shm_paddr as u64, 4 * PGSIZE)?;

        let mut device = DeviceVariant::Net(net_device);
        let mac = device.mac_address();
        let config = Config::new(HardwareAddress::Ethernet(mac));
        let time = smoltcp::time::Instant::from_micros(0);

        let mut iface = Interface::new(config, &mut device, time);
        iface.update_ip_addrs(|addrs| {
            addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)).unwrap();
        });
        iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2)).unwrap();

        self.interfaces.push(InterfaceContext { device, iface });
        self.probed_hardware.insert(hw_id);

        Ok(())
    }
}
