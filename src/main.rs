mod ai;
mod config;
mod handlers;
mod pipeline;
mod reporting;
mod store;
mod util;

use std::collections::HashMap;
use std::sync::Arc;

use teloxide::prelude::*;
use tokio::sync::Mutex;

use crate::ai::{Ai, generate_session_id};
use crate::config::Config;
use crate::pipeline::App;
use crate::store::Store;

#[tokio::main]
async fn main() {
    pretty_env_logger::init();
    dotenvy::dotenv().ok();

    let cfg = match Config::from_env() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            log::error!("{e}");
            std::process::exit(1);
        }
    };

    let bot = teloxide::Bot::new(cfg.teloxide_token.clone());
    let me = match bot.get_me().await {
        Ok(m) => m,
        Err(e) => {
            log::error!("Telegram getMe failed: {e}");
            std::process::exit(1);
        }
    };
    log::info!("Running as @{}", me.username());

    if cfg.ai_api_key.is_empty() && cfg.ai_base_url.contains("api.openai.com") {
        log::warn!(
            "AI_API_KEY is empty while using the OpenAI endpoint — analyses will fail until you set a key (or point AI_BASE_URL at a local server)"
        );
    }

    let data_dir = std::path::PathBuf::from(&cfg.data_dir);
    if let Err(e) = tokio::fs::create_dir_all(data_dir.join("media")).await {
        log::error!("can't create data dir: {e}");
        std::process::exit(1);
    }

    let mut store = Store::load(&data_dir.join("log.json"));

    // opencode (opencode.ai) requires a stable session id per conversation;
    // generate one once and persist it so it survives restarts.
    let ai_session_id = match store.ai_session_id().map(str::to_string) {
        Some(id) => id,
        None => {
            let id = generate_session_id();
            store.set_ai_session_id(&id);
            store.save();
            id
        }
    };
    if cfg.ai_base_url.contains("opencode.ai") {
        log::info!("opencode provider detected — sending x-opencode-session: {ai_session_id}");
    }

    let app = Arc::new(App {
        cfg: cfg.clone(),
        ai: Arc::new(Ai::new(
            cfg.ai_api_key.clone(),
            cfg.ai_base_url.clone(),
            cfg.ai_model.clone(),
            ai_session_id,
            cfg.ai_language.clone(),
        )),
        store: Mutex::new(store),
        bot_user_id: me.user.id,
        media_dir: data_dir.join("media"),
        albums: Mutex::new(HashMap::new()),
    });

    // Scheduled day/week/month reports.
    reporting::spawn_scheduler(bot.clone(), app.clone());

    // Retry loop for posts whose analysis failed.
    tokio::spawn({
        let bot = bot.clone();
        let app = app.clone();
        async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                pipeline::janitor_once(&bot, &app).await;
            }
        }
    });

    let handler = dptree::entry()
        .branch(Update::filter_channel_post().endpoint(handlers::channel_post))
        .branch(Update::filter_edited_channel_post().endpoint(handlers::channel_post_edited))
        .branch(
            Update::filter_message()
                .filter(|msg: Message| msg.chat.is_private())
                .endpoint(handlers::private_message),
        );

    log::info!("Starting dispatcher…");
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![app])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}
