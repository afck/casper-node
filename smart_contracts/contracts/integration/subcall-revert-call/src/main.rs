#![no_std]
#![no_main]

use casper_contract::{contract_api::runtime, unwrap_or_revert::UnwrapOrRevert};
use casper_types::{contracts::DEFAULT_ENTRY_POINT_NAME, ApiError, RuntimeArgs};

const REVERT_TEST_KEY: &str = "revert_test";

#[no_mangle]
pub extern "C" fn call() {
    let contract_hash = runtime::get_key(REVERT_TEST_KEY)
        .unwrap_or_revert_with(ApiError::GetKey)
        .into_hash()
        .unwrap_or_revert();

    runtime::call_contract(
        contract_hash,
        DEFAULT_ENTRY_POINT_NAME,
        RuntimeArgs::default(),
    )
}
