use std::{env, error::Error, time::Duration};

use mc_motd::{MinecraftMotd, query_bedrock_addr_into, query_bedrock_address};
use tokio::net::lookup_host;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let address = env::args()
        .nth(1)
        .unwrap_or_else(|| "nethernet.playhive.com:19132".to_string());

    // Bedrock-only automatic probing: RakNet and NetherNet race concurrently,
    // NetherNet is preferred, and the winning transport is cached. Java is
    // never contacted.
    let result = query_bedrock_address(&address, Duration::from_secs(3)).await?;
    println!(
        "transport={} latency={:?} host={} port={}",
        result.transport, result.latency, result.host, result.port
    );
    match &result.motd {
        MinecraftMotd::Bedrock(motd) => println!(
            "{} - {} ({}/{})",
            motd.motd_line_1,
            motd.version,
            players(motd.players_online),
            players(motd.players_max)
        ),
        MinecraftMotd::BedrockNetherNet(motd) => println!(
            "{} - {} level={} ({}/{}) mode={}",
            motd.name,
            motd.version,
            motd.level,
            players(motd.players_online),
            players(motd.players_max),
            motd.game_type_name().unwrap_or("unknown")
        ),
        _ => {}
    }

    // Hot polling path: one reusable buffer and borrowed parsing; the call
    // itself performs no heap allocation. It talks RakNet directly to the
    // resolved address, so it suits frequent polls of a known server.
    let target = format!("{}:19132", result.host);
    if let Ok(mut resolved) = lookup_host(&target).await {
        if let Some(sock) = resolved.next() {
            let mut buffer = vec![0_u8; 2048];
            for poll in 1..=3 {
                match query_bedrock_addr_into(sock, &mut buffer, Duration::from_secs(2)).await {
                    Ok(motd) => println!(
                        "poll {poll}: {} - {} ({}/{})",
                        motd.motd_line_1,
                        motd.version,
                        players(motd.players_online),
                        players(motd.players_max)
                    ),
                    Err(error) => println!("poll {poll} failed: {error}"),
                }
            }
        }
    }

    Ok(())
}

fn players(count: Option<u32>) -> String {
    count.map_or_else(|| "?".to_string(), |value| value.to_string())
}
