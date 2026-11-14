use std::{env, error::Error};

use mc_motd::{Details, query};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let address = env::args()
        .nth(1)
        .unwrap_or_else(|| "play.example.com".to_string());

    // One line: Java and Bedrock probes run concurrently, first response wins.
    let status = query(&address).await?;

    println!(
        "transport={:?} elapsed={:?} address={}",
        status.transport(),
        status.elapsed,
        status.address.host
    );
    println!("motd: {}", status.motd);
    println!(
        "players: {}/{}",
        status
            .players
            .online
            .map_or_else(|| "?".to_string(), |value| value.to_string()),
        status
            .players
            .max
            .map_or_else(|| "?".to_string(), |value| value.to_string())
    );
    println!("version: {:?}", status.version.name);

    match &status.details {
        Details::Java(details) => println!("{}", details.raw_json),
        Details::Bedrock(bedrock) => println!("{bedrock:?}"),
        // The enum is non_exhaustive so future protocols fit without another
        // breaking release.
        _ => {}
    }

    Ok(())
}
