#![no_std]
#![no_main]

extern crate alloc;

mod device;
mod gopher;
mod layout;

use glenda::cap::{CapPtr, Endpoint};
use glenda::cap::{RECV_SLOT, REPLY_SLOT};
use glenda::interface::SystemService;
use layout::{NET_CAP, TIMER_CAP};

pub use device::GlendaNetDevice;
pub use gopher::GopherManager;

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => ({
        glenda::println!("{}Gopher: {}{}", glenda::console::ANSI_BLUE, format_args!($($arg)*), glenda::console::ANSI_RESET);
    })
}

#[unsafe(no_mangle)]
fn main() -> usize {
    log!("Starting Network Stack...");

    // Service listening endpoint (conventionally at slot 16 for spawned services)
    let service_ep = Endpoint::from(CapPtr::from(16));

    let mut manager = GopherManager::new(NET_CAP, TIMER_CAP);

    if let Err(e) = manager.listen(service_ep, REPLY_SLOT) {
        log!("Failed to listen on service endpoint: {:?}", e);
        return 1;
    }

    log!("Network stack ready and listening for requests.");

    if let Err(e) = manager.run() {
        log!("Service loop exited with error: {:?}", e);
        return 1;
    }

    0
}
