use std::{
	borrow::Cow,
	ffi::{CStr, c_char}
};
use parking_lot::Mutex;
pub use ygopro::managers::{
	config_manager::{ConfigManager, set_global as set_config_manager},
	data_manager::{DataManager, card_reader, script_reader, set_global as set_data_manager},
	deck_manager::{DeckManager, set_global as set_deck_manager}
};
pub use ygopro_data::{
	constants::{Attribute, Category, Linkmarkers, OT, Race, Type},
	data::{CoreCard, Card}
};
pub static SCRIPT_BUFFER: Mutex<[u8; 0x100000]> = Mutex::new([0u8; 0x100000]);

pub fn get_log_message(pduel: isize) -> String {
	let mut buffer: [u8; 1024] = [0u8; 1024];
	unsafe {
		ygopro_core_wrapper::get_log_message(pduel, buffer.as_mut_ptr());
	}
	let c_message: &CStr = unsafe { CStr::from_ptr(buffer.as_ptr() as *const c_char) };
	let msg: Cow<'_, str> = c_message.to_string_lossy();
	msg.to_string()
}

pub extern "C" fn core_message_handler(_pduel: isize, _message_type: u32) -> u32 {
    0
}
