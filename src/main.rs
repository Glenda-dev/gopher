#![no_std]
#![no_main]

#[macro_use]
extern crate glenda;

extern crate alloc;
mod device;
mod gopher;
mod layout;

use crate::gopher::GopherServer;
use crate::layout::{DEVICE_CAP, DEVICE_SLOT, INIT_CAP, INIT_SLOT, TIME_CAP, TIME_SLOT};
use glenda::cap::{
    CSPACE_CAP, CapType, ENDPOINT_CAP, ENDPOINT_SLOT, MONITOR_CAP, RECV_SLOT, REPLY_SLOT,
};
use glenda::client::{DeviceClient, InitClient, ResourceClient, TimeClient};
use glenda::interface::SystemService;
use glenda::interface::resource::ResourceService;
use glenda::ipc::Badge;
use glenda::protocol::resource::{DEVICE_ENDPOINT, INIT_ENDPOINT, ResourceType, TIME_ENDPOINT};
use glenda::utils::manager::CSpaceManager;

pub use device::GlendaNetDevice;

#[unsafe(no_mangle)]
fn main() -> usize {
    glenda::console::init_logging("Gopher");
    log!("Starting Network Stack...");

    let mut res_client = ResourceClient::new(MONITOR_CAP);

    // Get endpoints from fixed slots
    res_client
        .get_cap(Badge::null(), ResourceType::Endpoint, INIT_ENDPOINT, INIT_SLOT)
        .expect("Gopher: Failed to get init endpoint cap");
    let mut init_client = InitClient::new(INIT_CAP);

    res_client
        .get_cap(Badge::null(), ResourceType::Endpoint, DEVICE_ENDPOINT, DEVICE_SLOT)
        .expect("Gopher: Failed to get device endpoint cap");
    let mut dev_client = DeviceClient::new(DEVICE_CAP);

    res_client
        .get_cap(Badge::null(), ResourceType::Endpoint, TIME_ENDPOINT, TIME_SLOT)
        .expect("Gopher: Failed to get time endpoint cap");
    let mut time_client = TimeClient::new(TIME_CAP);

    let mut cspace = CSpaceManager::new(CSPACE_CAP, 16);

    // Alloc endpoint for Gopher service
    res_client
        .alloc(Badge::null(), CapType::Endpoint, 0, ENDPOINT_SLOT)
        .expect("Gopher: Failed to create endpoint cap");

    let mut server = GopherServer::new(
        &mut res_client,
        &mut cspace,
        &mut dev_client,
        &mut init_client,
        &mut time_client,
    );

    if let Err(e) = server.listen(ENDPOINT_CAP, RECV_SLOT, REPLY_SLOT) {
        error!("Server listen failed: {:?}", e);
        return 1;
    }
    if let Err(e) = server.init() {
        error!("Server initialization failed: {:?}", e);
        return 1;
    }
    if let Err(e) = server.run() {
        error!("Server run failed: {:?}", e);
        return 1;
    }
    usize::MAX
}
