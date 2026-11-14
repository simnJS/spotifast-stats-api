use spotifast_stats_api::config::Config;
use spotifast_stats_api::{AppState, app, db, spotify};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;
    let bind_addr = config.bind_addr.clone();
    let state = AppState::new(pool, config);

    spotify::spawn_poller(state.clone());

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!(%bind_addr, "listening");
    axum::serve(listener, app(state)).await?;
    Ok(())
}
