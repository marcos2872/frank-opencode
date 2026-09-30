mod api;
mod cli;
mod config;
mod daemon;
mod domain;
mod infra;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let cli = cli::Cli::parse();
    let mut cfg = config::AppConfig::load(cli.config.clone());
    if let Some(p) = cli.port {
        cfg.port = p;
    }

    if cli.enable {
        return daemon::enable(cfg.port, cli.config);
    }
    if cli.disable {
        return daemon::disable();
    }
    if cli.status {
        return daemon::status();
    }
    if cli.refresh {
        let db = infra::opencode::resolve_db_path(&cfg.opencode_bin);
        let state = api::server::AppState::new(cfg, db);
        let n = state.refresh().await.map_err(|e| anyhow::anyhow!(e))?;
        let aliases = state.aliases.read().await;
        println!(
            "catalog: {n} enabled models, {} gateway aliases",
            aliases.len()
        );
        for a in aliases.iter() {
            println!("  {} -> {}", a.gateway_id, a.opencode_ref);
        }
        return Ok(());
    }
    if cli.serve || cli.daemon_child {
        return serve(cfg).await;
    }

    // No flag: show status + hint.
    daemon::status()?;
    println!();
    println!("usage: frank-opencode --enable | --disable | --status | --serve");
    Ok(())
}

async fn serve(cfg: config::AppConfig) -> anyhow::Result<()> {
    let port = cfg.port;
    let db = infra::opencode::resolve_db_path(&cfg.opencode_bin);
    tracing::info!(db = %db.display(), port, "starting frank-opencode");
    let state = api::server::AppState::new(cfg, db);
    match state.refresh().await {
        Ok(n) => tracing::info!(models = n, "catalog loaded"),
        Err(e) => tracing::warn!("catalog load failed (will retry on restart): {e}"),
    }
    let app = api::server::router(state);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
