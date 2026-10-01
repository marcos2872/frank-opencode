use clap::Parser;
use frank_opencode::{api, cli, config, daemon, infra};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let cli = cli::Cli::parse();
    let mut cfg = config::AppConfig::load(cli.config.clone()).map_err(|e| anyhow::anyhow!(e))?;
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

    // Bind first so a stuck catalog fetch can't block boot; the catalog
    // loads in the background and /health reports degraded until then.
    // No periodic refresh: the catalog is read once at startup (update it
    // by restarting the gateway, or preview with `--refresh`).
    let app = api::server::router(state.clone());
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("listening on http://{addr}");
    let loader = state.clone();
    tokio::spawn(async move {
        match loader.refresh().await {
            Ok(n) => tracing::info!(models = n, "catalog loaded"),
            Err(e) => tracing::warn!("initial catalog load failed: {e}"),
        }
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// SIGTERM (from `--disable`) and Ctrl-C drain in-flight SSE streams.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
