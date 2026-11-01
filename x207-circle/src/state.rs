//! Contract root `X207Circle` (CT-006) and the `interface_version` view.

use near_sdk::near;

use crate::INTERFACE_VERSION;

#[near(contract_state)]
#[derive(Default)]
pub struct X207Circle {}

#[near]
impl X207Circle {
    pub fn interface_version(&self) -> String {
        INTERFACE_VERSION.to_string()
    }
}
