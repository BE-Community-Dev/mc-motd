use std::{env, error::Error, time::Duration};

use mc_motd::{MinecraftMotd, query_minecraft_address};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let address = env::args()
        .nth(1)
        .unwrap_or_else(|| "play.example.com".to_string());
    let result = query_minecraft_address(address, Duration::from_secs(3)).await?;

    println!(
        "edition={:?} transport={} latency={:?} route={:?} host={} port={}",
        result.edition, result.transport, result.latency, result.route, result.host, result.port
    );

    match result.motd {
        MinecraftMotd::Java(status) => {
            println!("{}", status.raw_json);
        }
        MinecraftMotd::Bedrock(motd) => {
            println!(
                "{} - {} ({}/{})",
                motd.motd_line_1,
                motd.version,
                player_text(motd.players_online),
                player_text(motd.players_max)
            );
        }
        MinecraftMotd::BedrockNetherNet(motd) => {
            println!(
                "{} - {} level={} ({}/{}) mode={}",
                motd.name,
                motd.version,
                motd.level,
                player_text(motd.players_online),
                player_text(motd.players_max),
                motd.game_type_name().unwrap_or("unknown")
            );
            println!("note: latency is HTTP signaling time, not game RTT");
        }
        // The enum is non_exhaustive so future editions can be added without
        // another breaking release.
        _ => {}
    }

    Ok(())
}

fn player_text(players: Option<u32>) -> String {
    players.map_or_else(|| "?".to_string(), |value| value.to_string())
}
