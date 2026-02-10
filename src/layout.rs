use glenda::cap::{CapPtr, Endpoint};

pub const NET_SLOT: CapPtr = CapPtr::from(0);
pub const TIMER_SLOT: CapPtr = CapPtr::from(0);

pub const NET_CAP: Endpoint = Endpoint::from(NET_SLOT);
pub const TIMER_CAP: Endpoint = Endpoint::from(TIMER_SLOT);
