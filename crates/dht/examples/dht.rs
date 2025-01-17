use std::{str::FromStr, time::Duration};

use anyhow::Context;
use librqbit_dht::{Dht, Id20};
use tokio_stream::StreamExt;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let info_hash = Id20::from_str("64a980abe6e448226bb930ba061592e44c3781a1").unwrap();
    tracing_subscriber::fmt::init();

    let dht = Dht::new().await.context("error initializing DHT")?;
    let mut stream = dht.get_peers(info_hash).await?;

    let stats_printer = async {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            info!("DHT stats: {:?}", dht.stats());
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };

    let routing_table_dumper = async {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            dht.with_routing_table(|r| {
                let filename = "/tmp/routing-table.json";
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .open(filename)
                    .unwrap();
                serde_json::to_writer_pretty(&mut f, r).unwrap();
                info!("Dumped DHT routing table to {}", filename);
            });
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    };

    let peer_printer = async {
        while let Some(peer) = stream.next().await {
            info!("peer found: {}", peer)
        }
        Ok(())
    };

    let res = tokio::select! {
        res = stats_printer => res,
        res = peer_printer => res,
        res = routing_table_dumper => res,
    };
    res
}
