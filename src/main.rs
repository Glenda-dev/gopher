#![no_std]
#![no_main]

#[macro_use]
extern crate glenda;

extern crate alloc;
mod device;
mod gopher;
mod layout;

use crate::gopher::GopherServer;
use crate::layout::{DEVICE_CAP, DEVICE_SLOT, INIT_CAP, INIT_SLOT};
use glenda::cap::{
    CSPACE_CAP, CapType, ENDPOINT_CAP, Endpoint, MONITOR_CAP, RECV_SLOT, REPLY_SLOT,
};
use glenda::client::{DeviceClient, InitClient, ProcessClient, ResourceClient};
use glenda::interface::SystemService;
use glenda::interface::resource::ResourceService;
use glenda::ipc::Badge;
use glenda::protocol::resource::{DEVICE_ENDPOINT, INIT_ENDPOINT, ResourceType};
use glenda::utils::manager::{CSpaceManager, CSpaceService};

pub use device::GlendaNetDevice;

#[unsafe(no_mangle)]
fn main() -> usize {
    glenda::console::init_logging("Gopher");
    log!("Starting Network Stack...");

    let mut res_client = ResourceClient::new(MONITOR_CAP);
    let mut process_client = ProcessClient::new(MONITOR_CAP);

    // Get Init and Device endpoints from fixed slots (defined in layout)
    res_client
        .get_cap(Badge::null(), ResourceType::Endpoint, INIT_ENDPOINT, INIT_SLOT)
        .expect("Gopher: Failed to get init endpoint cap");
    let mut init_client = InitClient::new(INIT_CAP);

    res_client
        .get_cap(Badge::null(), ResourceType::Endpoint, DEVICE_ENDPOINT, DEVICE_SLOT)
        .expect("Gopher: Failed to get device endpoint cap");
    let mut dev_client = DeviceClient::new(DEVICE_CAP);

    let mut cspace = CSpaceManager::new(CSPACE_CAP, 16);

    // Alloc endpoint for Gopher service
    let endpoint_slot =
        cspace.alloc(&mut res_client).expect("Gopher: Failed to alloc endpoint slot");
    res_client
        .alloc(Badge::null(), CapType::Endpoint, 0, endpoint_slot)
        .expect("Gopher: Failed to create endpoint cap");
    let ep = Endpoint::from(endpoint_slot);

    log!("Starting server loop...");
    let mut server = GopherServer::new(
        ep,
        &mut res_client,
        &mut process_client,
        &mut cspace,
        &mut dev_client,
        &mut init_client,
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
