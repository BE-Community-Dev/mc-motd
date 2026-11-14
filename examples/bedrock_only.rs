use std::{env, error::Error, time::Duration};

#[cfg(feature = "nethernet")]
use mc_motd::protocol;
use mc_motd::{Client, Details, bedrock};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let address = env::args()
        .nth(1)
        .unwrap_or_else(|| "nethernet.playhive.com:19132".to_string());

    // Bedrock-only automatic probing: RakNet and NetherNet race concurrently,
    // the faster transport wins, and the winning transport is cached inside
    // the client. Java is never contacted.
    let client = Client::builder()
        .timeout(Duration::from_secs(3))
        .cache_capacity(256)
        .build();
    let status = client.bedrock(&address).await?;

    println!(
        "transport={:?} elapsed={:?} host={}",
        status.transport(),
        status.elapsed,
        status.address.host
    );
    match &status.details {
        Details::Bedrock(mc_motd::BedrockDetails::RakNet(details)) => println!(
            "raknet: {} - {} ({}/{})",
            details.motd_line_1,
            details.version,
            details.players_online.unwrap_or(0),
            details.players_max.unwrap_or(0)
        ),
        #[cfg(feature = "nethernet")]
        Details::Bedrock(mc_motd::BedrockDetails::NetherNet(details)) => println!(
            "nethernet: {} - {} level={} ({}/{}) mode={}",
            details.name,
            details.version,
            details.level,
            details.players_online.unwrap_or(0),
            details.players_max.unwrap_or(0),
            protocol::bedrock::nethernet::game_type_name(details.game_type).unwrap_or("unknown")
        ),
        _ => {}
    }

    // Forced-transport queries and zero-copy polling:
    let raknet = bedrock::raknet::query(&address).await?;
    println!("forced raknet: {}", raknet.motd);

    Ok(())
}
