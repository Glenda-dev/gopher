pub const RING_VA: usize = 0x8000_0000;
pub const SHM_VA: usize = 0x9000_0000;

use glenda::cap::{CapPtr, Endpoint};

pub const INIT_SLOT: CapPtr = CapPtr::from(14);
pub const DEVICE_SLOT: CapPtr = CapPtr::from(15);
pub const INIT_CAP: Endpoint = Endpoint::from(INIT_SLOT);
pub const DEVICE_CAP: Endpoint = Endpoint::from(DEVICE_SLOT);
