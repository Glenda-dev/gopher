use glenda::cap::Endpoint;
use glenda::client::device::net::NetClient;
use glenda::interface::NetDevice;
use smoltcp::phy;
use smoltcp::phy::{Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

pub struct GlendaNetDevice {
    client: NetClient,
}

impl GlendaNetDevice {
    pub fn new(cap: Endpoint) -> Self {
        Self { client: NetClient::new(cap) }
    }
}

pub struct RxToken {
    buffer: [u8; 2048],
    len: usize,
}

impl phy::RxToken for RxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.buffer[..self.len])
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
        let mut buffer = [0u8; 2048];
        let result = f(&mut buffer[..len]);
        let _ = self.client.send(&buffer[..len]);
        result
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
        let mut buffer = [0u8; 2048];
        match self.client.recv(&mut buffer) {
            Ok(len) => {
                let rx = RxToken { buffer, len };
                let tx = TxToken { client: &mut self.client };
                Some((rx, tx))
            }
            Err(_) => None,
        }
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
