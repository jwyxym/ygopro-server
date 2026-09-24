use parking_lot::{Mutex, MutexGuard};
use std::{
	io::{Error, ErrorKind, Result},
	sync::OnceLock,
	sync::mpsc::{Receiver as StartResultReceiver, Sender as StartResultSender, channel},
	thread::{JoinHandle, spawn},
	time::Duration,
	ffi::{c_char, c_int}
};
use tokio::{
	net::TcpListener,
	runtime::{Builder, Runtime},
	select,
	sync::oneshot::{
		Receiver as ShutdownReceiver,
		Sender as ShutdownSender,
		channel as shutdown_channel,
	},
};
use ygopro::{
	DuelHost,
	managers::{
		config_manager::{ConfigManager, set_global as set_config_manager},
		data_manager::{DataManager, set_global as set_data_manager},
		deck_manager::{DeckManager, set_global as set_deck_manager},
	},
	cli::{
		build_duel_host,
		start_local_server_with_listener
	}
};
use ygopro_data::{
	constants::{MasterRule, Mode, Rule},
	data::{ReplayMode, CoreCard},
	message::HostInfo,
};
use ygopro_core_wrapper::{
	set_card_reader,
	set_message_handler,
	set_script_reader,
	random::SEED_COUNT
};

static SERVER_CONTROL: OnceLock<Mutex<Option<ServerControl>>> = OnceLock::new();

struct ServerControl {
	shutdown_sender: ShutdownSender<()>,
	server_thread: JoinHandle<()>,
}

pub fn start (
	lflist: u32,
	rule: u8,
	mode: u8,
	replay_mode: u32,
	duel_rule: bool,
	no_check_deck: bool,
	no_shuffle_deck: bool,
	start_lp: u32,
	start_hand: u8,
	draw_count: u8,
	time_limit: u16,
	data_manager: DataManager,
	deck_manager: DeckManager,
	config_manager: ConfigManager,
	script_reader: Option<extern "C" fn(*const c_char, *mut c_int) -> *mut u8>,
	card_reader: Option<extern "C" fn(u32, *mut CoreCard) -> u32>,
	message_handler: Option<extern "C" fn(isize, u32) -> u32>
) -> Result<u16> {
	let seeds: Vec<[u32; SEED_COUNT]> = Vec::new();
	let replay_mode: ReplayMode = ReplayMode::from_bits_retain(replay_mode);
	let duel_rule: MasterRule = if duel_rule {
		MasterRule::MasterRuleNew
	} else {
		MasterRule::MasterRule2020
	};
	let mode: Mode = Mode::try_from(mode).unwrap_or(Mode::Single);
	let host_info: HostInfo = HostInfo {
		lflist: lflist,
		rule: Rule::try_from(rule).unwrap_or(Rule::All),
		duel_rule,
		no_check_deck,
		no_shuffle_deck,
		start_lp,
		start_hand,
		draw_count,
		time_limit,
		mode,
	};

	let server_control_lock: &Mutex<Option<ServerControl>> =
		SERVER_CONTROL.get_or_init(|| Mutex::new(None));
	let mut server_control: MutexGuard<'_, Option<ServerControl>> = server_control_lock.lock();
	let old_server_control: Option<ServerControl> = server_control.take();
	if let Some(i) = old_server_control {
		let i: ServerControl = i;
		i.shutdown_sender.send(()).ok();
		i.server_thread.join().ok();
	}

	let (shutdown_sender, shutdown_receiver): (ShutdownSender<()>, ShutdownReceiver<()>) =
		shutdown_channel();
	let (start_result_sender, start_result_receiver): (
		StartResultSender<Result<u16>>,
		StartResultReceiver<Result<u16>>,
	) = channel();
	let server_thread: JoinHandle<()> = spawn(move || {
		let runtime: Runtime = match Builder::new_multi_thread()
			.enable_all()
			.build()
		{
			Ok(runtime) => runtime,
			Err(error) => {
				start_result_sender.send(Err(error)).ok();
				return;
			}
		};

		runtime.block_on(async move {
			init(
				data_manager,
				deck_manager,
				config_manager,
				script_reader,
				card_reader,
				message_handler
			)
			.await;
			let run_result: Result<()> = run_tcp_server(
				replay_mode,
				host_info,
				seeds,
				shutdown_receiver,
				&start_result_sender,
			)
			.await;
			if let Err(error) = run_result {
				start_result_sender.send(Err(error)).ok();
			}
		});
		runtime.shutdown_timeout(Duration::from_secs(2));
	});

	let start_result = start_result_receiver
		.recv()
		.map_err(|error| Error::new(ErrorKind::BrokenPipe, error))
		.and_then(|result| result);
	match start_result {
		Ok(port) => {
			*server_control = Some(ServerControl {
				shutdown_sender,
				server_thread,
			});
			Ok(port)
		}
		Err(error) => {
			shutdown_sender.send(()).ok();
			server_thread.join().ok();
			Err(error)
		}
	}
}

pub fn stop () {
	let server_control: Option<ServerControl> = {
		let server_control_lock: &Mutex<Option<ServerControl>> =
			SERVER_CONTROL.get_or_init(|| Mutex::new(None));
		server_control_lock.lock().take()
	};

	if let Some(server_control) = server_control {
		let server_control: ServerControl = server_control;
		server_control.shutdown_sender.send(()).ok();
		server_control.server_thread.join().ok();
	}
}

async fn run_tcp_server (
	replay_mode: ReplayMode,
	host_info: HostInfo,
	seeds: Vec<[u32; SEED_COUNT]>,
	shutdown_receiver: ShutdownReceiver<()>,
	start_result_sender: &StartResultSender<Result<u16>>,
) -> Result<()> {
	let listener: TcpListener = TcpListener::bind("0.0.0.0:0").await?;
	let port: u16 = listener.local_addr()?.port();

	start_result_sender.send(Ok(port)).ok();

	let duel: DuelHost = build_duel_host(host_info, replay_mode, seeds);
	select! {
		_ = shutdown_receiver => {}
		_ = start_local_server_with_listener(listener, duel) => {}
	}

	Ok(())
}

async fn init (
	data_manager: DataManager,
	deck_manager: DeckManager,
	config_manager: ConfigManager,
	script_reader: Option<extern "C" fn(*const c_char, *mut c_int) -> *mut u8>,
	card_reader: Option<extern "C" fn(u32, *mut CoreCard) -> u32>,
	message_handler: Option<extern "C" fn(isize, u32) -> u32>
) -> () {
	set_config_manager(config_manager);
	set_data_manager(data_manager);
	set_deck_manager(deck_manager);
	unsafe {
		set_script_reader(script_reader);
		set_card_reader(card_reader);
		set_message_handler(message_handler);
	}
}