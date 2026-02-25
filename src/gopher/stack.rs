use crate::GlendaNetDevice;
use smoltcp::iface::Interface;
use smoltcp::time::Instant;

pub enum DeviceVariant {
    Net(GlendaNetDevice),
    Loopback(smoltcp::phy::Loopback),
}

impl smoltcp::phy::Device for DeviceVariant {
    type RxToken<'ax>
        = RxVariant<'ax>
    where
        Self: 'ax;
    type TxToken<'ax>
        = TxVariant<'ax>
    where
        Self: 'ax;

    fn receive(&mut self, timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        match self {
            Self::Net(d) => {
                d.receive(timestamp).map(|(rx, tx)| (RxVariant::Net(rx), TxVariant::Net(tx)))
            }
            Self::Loopback(d) => d
                .receive(timestamp)
                .map(|(rx, tx)| (RxVariant::Loopback(rx), TxVariant::Loopback(tx))),
        }
    }

    fn transmit(&mut self, timestamp: Instant) -> Option<Self::TxToken<'_>> {
        match self {
            Self::Net(d) => d.transmit(timestamp).map(|tx| TxVariant::Net(tx)),
            Self::Loopback(d) => d.transmit(timestamp).map(|tx| TxVariant::Loopback(tx)),
        }
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        match self {
            Self::Net(d) => d.capabilities(),
            Self::Loopback(d) => d.capabilities(),
        }
    }
}

pub enum RxVariant<'a> {
    Net(crate::device::RxToken),
    Loopback(<smoltcp::phy::Loopback as smoltcp::phy::Device>::RxToken<'a>),
}

impl<'a> smoltcp::phy::RxToken for RxVariant<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        match self {
            Self::Net(t) => t.consume(f),
            Self::Loopback(t) => t.consume(f),
        }
    }
}

pub enum TxVariant<'a> {
    Net(crate::device::TxToken<'a>),
    Loopback(<smoltcp::phy::Loopback as smoltcp::phy::Device>::TxToken<'a>),
}

impl<'a> smoltcp::phy::TxToken for TxVariant<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        match self {
            Self::Net(t) => t.consume(len, f),
            Self::Loopback(t) => t.consume(len, f),
        }
    }
}

pub struct InterfaceContext {
    pub device: DeviceVariant,
    pub iface: Interface,
}
