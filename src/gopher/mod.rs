use crate::device::GlendaNetDevice;
use crate::layout::{RING_VA, SHM_VA};
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use config::*;
use core::sync::atomic::{AtomicUsize, Ordering};
use glenda::arch::mem::PGSIZE;
use glenda::cap::{CapPtr, Endpoint, Reply};
use glenda::client::{DeviceClient, InitClient, ProcessClient, ResourceClient};
use glenda::error::Error;
use glenda::interface::device::DeviceService;
use glenda::interface::memory::MemoryService;
use glenda::io::uring::IoUringServer;
use glenda::ipc::Badge;
use glenda::protocol::device::LogicDeviceType;
use glenda::utils::manager::{CSpaceManager, CSpaceService};
use glenda_drivers::interface::NetDriver;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address};
use stack::{DeviceVariant, InterfaceContext};

pub mod config;
pub mod network;
pub mod server;
pub mod stack;

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

    pub shm_frame: Option<(glenda::cap::Frame, usize, usize, usize)>, // Frame, vaddr, size, paddr
    pub config: Option<NetworkConfig>,
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
            shm_frame: None,
            config: None,
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
            dev_type: Some(3), // Type 3 = Net
        };
        let names = self.device_client.query(Badge::null(), query)?;
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
                    log!("Found new network device to probe: {} (hw_id={:x})", name, hw_id);
                    let hw_slot = self.cspace.alloc(self.res_client)?;
                    let hw_ep = self.device_client.alloc_logic(Badge::null(), 3, &name, hw_slot)?;
                    self.probe(&name, hw_id, desc, hw_ep)?;
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
        name: &str,
        hw_id: u64,
        desc: glenda::protocol::device::LogicDeviceDesc,
        hardware_ep: Endpoint,
    ) -> Result<(), Error> {
        log!("Probing network device {} (hw_id={:x})", desc.name, hw_id);

        let mut net_device = GlendaNetDevice::new(hardware_ep, &desc.name);

        // Setup IO ring
        let ring_va = self.next_ring_vaddr.fetch_add(PGSIZE, Ordering::SeqCst);
        let ring_frame = net_device.setup_ring(4, 4, self.endpoint, self.recv)?;
        MemoryService::mmap(self.res_client, Badge::null(), ring_frame, ring_va, PGSIZE)?;

        // Setup SHM from global pool
        if let Some((shm_frame, shm_va, shm_size, shm_paddr)) = &self.shm_frame {
            net_device.setup_shm(shm_frame.clone(), *shm_va, *shm_paddr as u64, *shm_size)?;
        } else {
            error!("Global SHM not initialized");
            return Err(Error::NotInitialized);
        }
        let mut device = DeviceVariant::Net(net_device);
        let mac = device.mac_address();
        let config = Config::new(HardwareAddress::Ethernet(mac));
        let time = smoltcp::time::Instant::from_micros(0);

        let mut iface = Interface::new(config, &mut device, time);
        log!("Probed device {} with MAC {}", name, mac);
        // Apply configuration from network.json if available
        let mut configured = false;
        if let Some(config) = &self.config {
            if let Some(iface_config) = config.interfaces.iter().find(|i| i.name == name) {
                if let Ok(addr) = iface_config.ipv4.parse::<Ipv4Address>() {
                    iface.update_ip_addrs(|addrs| {
                        log!(
                            "Configuring interface {} with IP {}/{}",
                            name,
                            addr,
                            iface_config.mask
                        );
                        addrs.push(IpCidr::new(IpAddress::Ipv4(addr), iface_config.mask)).unwrap();
                    });
                    if let Some(gw) = &iface_config.gateway {
                        if let Ok(gw_addr) = gw.parse::<Ipv4Address>() {
                            log!("Setting default gateway for {} to {}", name, gw_addr);
                            iface.routes_mut().add_default_ipv4_route(gw_addr).unwrap();
                        }
                    }
                    configured = true;
                }
            }

            // Apply global routes
            for route in &config.routes {
                if let (Ok(dest), Ok(via)) =
                    (route.dest.parse::<Ipv4Address>(), route.via.parse::<Ipv4Address>())
                {
                    if dest.is_unspecified() && route.mask == 0 {
                        log!("Adding default route via {}", via);
                        iface.routes_mut().add_default_ipv4_route(via).unwrap();
                    }
                }
            }
        }

        if !configured {
            // Default fallback
            iface.update_ip_addrs(|addrs| {
                addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)).unwrap();
            });
            iface.routes_mut().add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2)).unwrap();
        }

        self.interfaces.push(InterfaceContext { device, iface });
        self.probed_hardware.insert(hw_id);

        Ok(())
    }
}
