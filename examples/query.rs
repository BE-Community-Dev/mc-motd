use std::{env, error::Error, time::Duration};

use mc_motd::{MinecraftMotd, query_minecraft_address};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let address = env::args()
        .nth(1)
        .unwrap_or_else(|| "play.example.com".to_string());
    let result = query_minecraft_address(address, Duration::from_secs(3)).await?;

    println!(
        "edition={:?} route={:?} host={} port={}",
        result.edition, result.route, result.host, result.port
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
                motd.players_online
                    .map_or_else(|| "?".to_string(), |value| value.to_string()),
                motd.players_max
                    .map_or_else(|| "?".to_string(), |value| value.to_string())
            );
        }
    }

    Ok(())
}
