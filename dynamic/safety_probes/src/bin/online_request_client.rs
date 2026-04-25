use hbb_common::{
    bytes::Bytes,
    bytes_codec::BytesCodec,
    futures_util::{SinkExt, StreamExt},
    protobuf::Message as _,
    rendezvous_proto::{rendezvous_message, OnlineRequest, RendezvousMessage},
    tokio::{net::TcpStream, time},
    tokio_util::codec::Framed,
};
use std::{error::Error, time::Duration};

fn main() -> Result<(), Box<dyn Error>> {
    hbb_common::tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<(), Box<dyn Error>> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:21115".to_owned());
    let peer_count = std::env::args()
        .nth(2)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(10_000);
    let licence_key = std::env::args().nth(3).unwrap_or_default();

    let peers = (0..peer_count)
        .map(|i| format!("safety-peer-{i:08}"))
        .collect::<Vec<_>>();

    let mut msg = RendezvousMessage::new();
    msg.set_online_request(OnlineRequest {
        id: "safety-probe".to_owned(),
        peers,
        licence_key,
        ..Default::default()
    });

    let request_bytes = msg.write_to_bytes()?;
    let request_len = request_bytes.len();
    let stream = TcpStream::connect(&addr).await?;
    let mut framed = Framed::new(stream, BytesCodec::new());
    framed.send(Bytes::from(request_bytes)).await?;

    let frame = time::timeout(Duration::from_secs(5), framed.next()).await?;
    let Some(frame) = frame else {
        println!("addr={addr}");
        println!("peer_count={peer_count}");
        println!("request_len={request_len}");
        println!("response=closed");
        return Ok(());
    };
    let response_bytes = frame?;
    let response_len = response_bytes.len();
    let response = RendezvousMessage::parse_from_bytes(&response_bytes)?;

    println!("addr={addr}");
    println!("peer_count={peer_count}");
    println!("request_len={request_len}");
    println!("response_len={response_len}");
    match response.union {
        Some(rendezvous_message::Union::OnlineResponse(resp)) => {
            println!("online_states_len={}", resp.states.len());
        }
        other => {
            println!(
                "response_union={}",
                if other.is_some() { "other" } else { "none" }
            );
        }
    }

    Ok(())
}
