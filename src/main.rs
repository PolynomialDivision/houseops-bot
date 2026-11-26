mod config;
mod house;
mod matrix;
mod model;
mod parse;
mod reminders;
mod render;
mod store;
mod web;

use anyhow::{Context, Result};
use mxbot_common::{settings::Settings, Bot};
use tracing::{error, info, warn};

use crate::{config::Config, house::House, store::Store};

#[tokio::main]
async fn main() -> Result<()> {
    mxbot_common::logging::init("houseops_bot");

    let config: Config =
        mxbot_common::config::load_toml(&mxbot_common::config::config_path_from_args())?;
    let store_path = mxbot_common::config::store_path_from_env();

    let settings = Settings::load(
        store_path.join("settings.json"),
        config::Settings::from_config(&config.reminders),
    )
    .await?;

    let bot = Bot::builder("houseops-bot", env!("CARGO_PKG_VERSION"))
        .store_path(&store_path)
        .settings(settings.store().clone())
        .admin_help(config::ADMIN_HELP)
        .start(&config.matrix, &config.security)
        .await?;

    let store = Store::open(&store_path)
        .await
        .context("Opening the task store")?;
    let public_url = config
        .web
        .as_ref()
        .map(|w| w.public_url.trim_end_matches('/').to_owned());
    let house = House::new(bot.clone(), store, config.house, public_url, settings)?;

    matrix::install_handlers(&house);

    info!("Starting sync...");
    bot.initial_sync().await;
    if house.room().is_none() {
        warn!(
            "The bot is not in the house room {} yet: invite it there",
            house.room_id()
        );
    }

    if let Some(web) = config.web {
        let house = house.clone();
        tokio::spawn(async move {
            if let Err(e) = web::run(house, web.bind_addr).await {
                error!("Web interface stopped: {e:#}");
            }
        });
    } else {
        info!("No [web] section, web interface disabled");
    }
    tokio::spawn(reminders::run(house));

    bot.run().await
}
