use std::{env, error::Error, time::Duration};

use mc_motd::lan;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut options = lan::DiscoveryOptions::default().listen_duration(Duration::from_secs(2));

    // Pass a port argument to broadcast on a different port, e.g. 19132 for
    // vanilla Bedrock LAN worlds.
    if let Some(port) = env::args().nth(1) {
        options.broadcast_port = port.parse()?;
        println!("broadcasting on UDP port {}", options.broadcast_port);
    } else {
        println!(
            "broadcasting on UDP port {} (vanilla uses {})",
            options.broadcast_port,
            lan::VANILLA_PORT
        );
    }

    let servers = lan::discover(options).await?;
    if servers.is_empty() {
        println!("no LAN servers replied");
        return Ok(());
    }

    for server in servers {
        println!(
            "{}: {} ({}/{})",
            server.source,
            server.details.motd_line_1,
            server
                .details
                .players_online
                .map_or_else(|| "?".to_string(), |value| value.to_string()),
            server
                .details
                .players_max
                .map_or_else(|| "?".to_string(), |value| value.to_string())
        );
    }

    Ok(())
}
