use parking_lot::Mutex;
use std::{
	io::{Error, ErrorKind, Result},
	sync::OnceLock,
	sync::mpsc::{Receiver as StartResultReceiver, Sender as StartResultSender, channel},
	thread::JoinHandle,
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
static ACTIVE_SERVERS: Mutex<usize> = Mutex::new(0);

/// 独立服务器句柄；丢弃句柄会请求停止服务。
#[must_use = "keep the handle alive while the server is running"]
pub struct Server {
	port: u16,
	control: Option<ServerControl>,
}

impl Server {
	pub fn port(&self) -> u16 {
		self.port
	}

	/// 请求停止，并异步等待监听端口和运行时释放。
	pub async fn stop(mut self) -> Result<()> {
		let control = self.control.take().expect("server control is present");
		control.shutdown_sender.send(()).ok();
		tokio::task::spawn_blocking(move || {
			control.server_thread.join()
				.map_err(|_| Error::other("server thread panicked"))
		})
		.await
		.map_err(Error::other)?
	}
}

impl Drop for Server {
	fn drop(&mut self) {
		if let Some(control) = self.control.take() {
			control.shutdown_sender.send(()).ok();
		}
	}
}

struct ActiveServer;

impl Drop for ActiveServer {
	fn drop(&mut self) {
		*ACTIVE_SERVERS.lock() -= 1;
	}
}

struct ServerControl {
	shutdown_sender: ShutdownSender<()>,
	server_thread: JoinHandle<()>,
}

/// 同步启动单个服务，替换此前由本接口启动的服务。
///
/// 不会停止 `start_once` 启动的实例；全局资源共享规则同 `start_once`。
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
	let server_control_lock = SERVER_CONTROL.get_or_init(|| Mutex::new(None));
	let mut server_control = server_control_lock.lock();
	if let Some(control) = server_control.take() {
		control.shutdown_sender.send(()).ok();
		control.server_thread.join().ok();
	}
	let mut server = start_server(
		lflist, rule, mode, replay_mode, duel_rule, no_check_deck, no_shuffle_deck,
		start_lp, start_hand, draw_count, time_limit,
		data_manager, deck_manager, config_manager,
		script_reader, card_reader, message_handler,
	)?;
	*server_control = server.control.take();
	Ok(server.port)
}

/// 启动独立服务，不替换同步接口或其他异步接口启动的服务。
///
/// 同时运行的服务共用第一个活动服务设置的 managers 和核心回调。
/// 只有全部服务停止后，后续启动才会重新设置这些进程全局资源。
/// 对战参数和监听端口属于各自实例。返回的句柄必须保持存活。
/// 必须在 Tokio 运行时内调用；共享的核心回调需要支持多线程调用。
pub async fn start_once (
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
) -> Result<Server> {
	tokio::task::spawn_blocking(move || start_server(
		lflist, rule, mode, replay_mode, duel_rule, no_check_deck, no_shuffle_deck,
		start_lp, start_hand, draw_count, time_limit,
		data_manager, deck_manager, config_manager,
		script_reader, card_reader, message_handler,
	))
	.await
	.map_err(Error::other)?
}

fn start_server (
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
) -> Result<Server> {
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

	let (shutdown_sender, shutdown_receiver): (ShutdownSender<()>, ShutdownReceiver<()>) =
		shutdown_channel();
	let (start_result_sender, start_result_receiver): (
		StartResultSender<Result<u16>>,
		StartResultReceiver<Result<u16>>,
	) = channel();
	let server_thread: JoinHandle<()> = std::thread::Builder::new().spawn(move || {
		// 先声明资源占用标记，确保正常退出和 panic 时都先释放运行时。
		let active_server = init(
			data_manager,
			deck_manager,
			config_manager,
			script_reader,
			card_reader,
			message_handler
		);
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
		// 上游会派生监听和连接任务，释放整个实例的运行时才能完整停止。
		drop(runtime);
		drop(active_server);
	})?;

	let start_result = start_result_receiver
		.recv()
		.map_err(|error| Error::new(ErrorKind::BrokenPipe, error))
		.and_then(|result| result);
	match start_result {
		Ok(port) => {
			Ok(Server {
				port,
				control: Some(ServerControl {
					shutdown_sender,
					server_thread,
				}),
			})
		}
		Err(error) => {
			shutdown_sender.send(()).ok();
			server_thread.join().ok();
			Err(error)
		}
	}
}

/// 同步停止由 `start` 启动的服务，不影响独立的异步实例。
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

	let duel: DuelHost = build_duel_host(host_info, replay_mode, seeds);
	if start_result_sender.send(Ok(port)).is_err() {
		return Ok(());
	}
	select! {
		_ = shutdown_receiver => {}
		_ = start_local_server_with_listener(listener, duel) => {}
	}

	Ok(())
}

fn init (
	data_manager: DataManager,
	deck_manager: DeckManager,
	config_manager: ConfigManager,
	script_reader: Option<extern "C" fn(*const c_char, *mut c_int) -> *mut u8>,
	card_reader: Option<extern "C" fn(u32, *mut CoreCard) -> u32>,
	message_handler: Option<extern "C" fn(isize, u32) -> u32>
) -> ActiveServer {
	let mut active_servers = ACTIVE_SERVERS.lock();
	if *active_servers == 0 {
		set_config_manager(config_manager);
		set_data_manager(data_manager);
		set_deck_manager(deck_manager);
		unsafe {
			set_script_reader(script_reader);
			set_card_reader(card_reader);
			set_message_handler(message_handler);
		}
	}
	*active_servers += 1;
	ActiveServer
}
