use std::io::Result;
use ygopro_server::{Server, start_once};
use ygopro_server::defalut::{
	ConfigManager, DataManager, DeckManager,
	card_reader, script_reader, core_message_handler,
};

async fn start_one() -> Result<Server> {
	// 实际使用时，先加载卡片、脚本和禁限卡表。
	// 同时活动的服务共用第一个服务设置的 managers 和核心回调。
	start_once(
		0, 0, 0, 0, false, false, false,
		8000, 5, 1, 180,
		DataManager::new(), DeckManager::new(), ConfigManager::new(),
		Some(script_reader), Some(card_reader), Some(core_message_handler),
	).await
}

#[tokio::main]
async fn main() -> Result<()> {
	let first = start_one().await?;
	let second = start_one().await?;
	println!("servers: {}, {}", first.port(), second.port());

	// 可在这里等待业务逻辑或外部关闭信号；只需要一个服务时不启动 second。
	first.stop().await?;
	// 停止 first 后，second 仍然运行。
	second.stop().await?;
	Ok(())
}
