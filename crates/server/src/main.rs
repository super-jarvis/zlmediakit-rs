use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tracing::{info, error};
use tracing_subscriber::EnvFilter;

use zlmediakit_core::config::ServerConfig;
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_rtmp::RtmpServer;
use zlmediakit_rtsp::RtspServer;
use zlmediakit_http::HttpServer;

#[derive(Parser, Debug)]
#[command(name = "zlmediakit")]
#[command(about = "ZLMediaKit-RS - High performance streaming media server in Rust")]
struct Cli {
    #[arg(long, default_value = "conf/config.toml")]
    config: String,

    #[arg(long)]
    rtmp_port: Option<u16>,

    #[arg(long)]
    rtsp_port: Option<u16>,

    #[arg(long)]
    http_port: Option<u16>,

    #[arg(short = 'a', long)]
    api_port: Option<u16>,

    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(&cli.log_level)),
        )
        .init();

    let mut config = if std::path::Path::new(&cli.config).exists() {
        ServerConfig::load(&cli.config)?
    } else {
        info!("Config file {} not found, using defaults", cli.config);
        ServerConfig::default()
    };

    if let Some(port) = cli.rtmp_port {
        config.rtmp_port = port;
    }
    if let Some(port) = cli.rtsp_port {
        config.rtsp_port = port;
    }
    if let Some(port) = cli.http_port {
        config.http_port = port;
    }
    if let Some(port) = cli.api_port {
        config.api_port = port;
    }

    let source_manager = Arc::new(MediaSourceManager::new());

    info!("========================================");
    info!("  ZLMediaKit-RS v{}", env!("CARGO_PKG_VERSION"));
    info!("  High Performance Streaming Server");
    info!("========================================");
    info!("RTMP port: {}", config.rtmp_port);
    info!("RTSP port: {}", config.rtsp_port);
    info!("HTTP port: {}", config.http_port);
    info!("API port:  {}", config.api_port);

    let mut handles = Vec::new();

    if config.rtmp.enabled {
        let addr = format!("0.0.0.0:{}", config.rtmp_port);
        let sm = source_manager.clone();
        let handle = tokio::spawn(async move {
            match RtmpServer::new(&addr, sm).await {
                Ok(server) => {
                    if let Err(e) = server.run().await {
                        error!("RTMP server error: {}", e);
                    }
                }
                Err(e) => error!("Failed to start RTMP server: {}", e),
            }
        });
        handles.push(handle);
    }

    if config.rtsp.enabled {
        let addr = format!("0.0.0.0:{}", config.rtsp_port);
        let sm = source_manager.clone();
        let handle = tokio::spawn(async move {
            match RtspServer::new(&addr, sm).await {
                Ok(server) => {
                    if let Err(e) = server.run().await {
                        error!("RTSP server error: {}", e);
                    }
                }
                Err(e) => error!("Failed to start RTSP server: {}", e),
            }
        });
        handles.push(handle);
    }

    if config.http.enabled {
        let addr = format!("0.0.0.0:{}", config.http_port);
        let sm = source_manager.clone();
        let handle = tokio::spawn(async move {
            match HttpServer::new(&addr, sm).await {
                Ok(server) => {
                    if let Err(e) = server.run().await {
                        error!("HTTP server error: {}", e);
                    }
                }
                Err(e) => error!("Failed to start HTTP server: {}", e),
            }
        });
        handles.push(handle);
    }

    info!("All servers started. Press Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    for handle in handles {
        handle.abort();
    }

    Ok(())
}
