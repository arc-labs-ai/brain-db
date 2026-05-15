//! SUBSCRIBE streaming-form smoke test.
//!
//! Each frame on the subscription stream carries exactly one
//! `SubscriptionEvent`. The stream is unbounded until the
//! caller drops it (or the server closes).

mod common;

use brain_protocol::opcode::Opcode;
use brain_protocol::request::MemoryKindWire;
use brain_protocol::response::{EventType, SubscriptionEvent};
use brain_protocol::{RequestBody, ResponseBody};
use brain_sdk_rust::Client;
use futures_lite::StreamExt;

fn event(idx: u64) -> SubscriptionEvent {
    SubscriptionEvent {
        event_type: EventType::Encoded,
        lsn: idx,
        memory_id: idx as u128,
        context_id: 0,
        kind: MemoryKindWire::Episodic,
        salience: 0.5,
        timestamp_unix_nanos: idx * 1_000_000_000,
        text: String::new(),
        knowledge_payload: None,
    }
}

#[tokio::test]
async fn subscribe_stream_yields_events_then_stops_when_dropped() {
    let (addr, _server) = common::spawn_mock_server(|mut socket| async move {
        let frame = common::read_frame(&mut socket).await;
        assert_eq!(frame.header.opcode_u16(), Opcode::SubscribeReq.as_u16());
        let _ = RequestBody::decode(Opcode::SubscribeReq, &frame.payload).expect("decode");
        let sid = frame.header.stream_id_u32();

        // Send 5 events, each in its own frame. None has EOS;
        // the stream is open until the client drops.
        for i in 0..5 {
            common::write_frame(
                &mut socket,
                Opcode::SubscribeEvent.as_u16(),
                sid,
                ResponseBody::SubscribeEvent(event(i)).encode(),
                false,
            )
            .await;
        }
        // After this, the mock idles. The client will read 3
        // events then drop the stream; the dropped stream
        // closes the connection, the mock detects EOF.
        let mut buf = [0u8; 64];
        loop {
            use tokio::io::AsyncReadExt;
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(_) => continue,
            }
        }
    })
    .await;

    let client = Client::connect(addr).await.expect("connect");
    let mut stream = client.subscribe().send_stream().await.expect("open stream");

    let mut lsns = Vec::new();
    for _ in 0..3 {
        let item = stream.next().await.expect("yielded").expect("ok");
        lsns.push(item.lsn);
    }
    assert_eq!(lsns, vec![0, 1, 2]);

    // Drop the stream; the connection is released back. Since
    // it's still mid-subscription (the mock has 2 more events
    // queued), the pool may not return the connection to a
    // pristine state. For 10.6 we accept this and document.
    drop(stream);

    // Don't call bye() here — the connection may still have
    // pending event frames queued by the mock. The test's job
    // is verifying the stream surface, not pool-after-stream
    // recovery (10.6's plan §5 risks document the limitation).
    let _ = tokio::time::timeout(std::time::Duration::from_millis(200), client.bye()).await;
}
