use glenda::cap::{CapPtr, Endpoint, Frame};
use glenda::error::Error;
use glenda::protocol::device::net::MacAddress;
use glenda_drivers::client::net::NetClient;
use glenda_drivers::interface::NetDriver;
use smoltcp::phy;
use smoltcp::phy::{Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

pub struct GlendaNetDevice {
    pub client: NetClient,
    pub rx_pending: bool,
    pub rx_id: u64,
    pub name: alloc::string::String,
}

impl GlendaNetDevice {
    pub fn new(cap: Endpoint, name: &str) -> Self {
        Self {
            client: NetClient::new(cap),
            rx_pending: false,
            rx_id: 0x100,
            name: alloc::string::String::from(name),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl NetDriver for GlendaNetDevice {
    fn mac_address(&self) -> MacAddress {
        self.client.mac_address()
    }

    fn setup_ring(
        &mut self,
        sq_entries: u32,
        cq_entries: u32,
        notify_ep: Endpoint,
        recv: CapPtr,
    ) -> Result<Frame, Error> {
        self.client.setup_ring(sq_entries, cq_entries, notify_ep, recv)
    }

    fn setup_shm(
        &mut self,
        frame: Frame,
        vaddr: usize,
        paddr: u64,
        size: usize,
    ) -> Result<(), Error> {
        self.client.setup_shm(frame, vaddr, paddr, size)
    }
}

pub struct RxToken {
    pub shm: *mut u8,
    pub shm_idx: usize,
    pub len: usize,
}

impl phy::RxToken for RxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let slice =
            unsafe { core::slice::from_raw_parts(self.shm.add(self.shm_idx * 4096), self.len) };
        f(slice)
    }
}

pub struct TxToken<'a> {
    client: &'a mut NetClient,
}

impl<'a> phy::TxToken for TxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        if let Some(shm) = self.client.shm() {
            let slice = unsafe { shm.as_mut_slice() };
            let result = f(&mut slice[..len]);
            let _ = self.client.send_packet(&slice[..len]);
            result
        } else {
            let mut buffer = [0u8; 2048];
            let result = f(&mut buffer[..len]);
            let _ = self.client.send_packet(&buffer[..len]);
            result
        }
    }
}

impl Device for GlendaNetDevice {
    type RxToken<'a>
        = RxToken
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken<'a>
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Submit an RX if none is pending
        if !self.rx_pending {
            if let Some(shm) = self.client.shm() {
                // Buffer offset 0 for RX (simplification)
                let slice = unsafe { &mut shm.as_mut_slice()[..2048] };
                if self.client.submit_recv(slice, self.rx_id).is_ok() {
                    self.rx_pending = true;
                }
            }
        }

        // Peek for RX completion
        if self.rx_pending {
            if let Some(cqe) = self.client.peek_cqe() {
                if cqe.user_data == self.rx_id {
                    self.rx_pending = false;
                    if cqe.res > 0 {
                        let len = cqe.res as usize;
                        let shm_ptr = self.client.shm().unwrap().as_ptr();
                        let rx = RxToken { shm: shm_ptr, shm_idx: 0, len };
                        let tx = TxToken { client: &mut self.client };
                        return Some((rx, tx));
                    }
                }
            }
        }
        None
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken { client: &mut self.client })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = Medium::Ethernet;
        caps
    }
}
