//! The connection check after the machine wakes (libsignal-service's
//! `SignalWebSocket::check`, as Desktop's `checkSockets`), against a local
//! server: one that doesn't answer is closed after the check's 30 seconds
//! rather than at the next keepalive, and one that answers stays open.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use presage::libsignal_service::{
    configuration::SignalServers,
    prelude::PushService,
    websocket::{Identified, SignalWebSocket},
};
use presage::proto::{WebSocketMessage, WebSocketResponseMessage, web_socket_message};
use prost::Message as _;
use reqwest_websocket::Upgrade;
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

/// A socket to a local server that reads everything and answers keepalives
/// only if `answer`.
async fn connect(answer: bool) -> SignalWebSocket<Identified> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        while let Some(Ok(message)) = ws.next().await {
            let Message::Binary(frame) = message else { continue };
            let Some(request) = WebSocketMessage::decode(frame).unwrap().request else { continue };
            if answer {
                let response = WebSocketMessage {
                    r#type: Some(web_socket_message::Type::Response.into()),
                    response: Some(WebSocketResponseMessage { id: request.id, status: Some(200), ..Default::default() }),
                    ..Default::default()
                };
                let _ = ws.send(Message::Binary(response.encode_to_vec().into())).await;
            }
        }
    });
    // First: it installs the TLS provider reqwest needs.
    let push_service = PushService::new(SignalServers::Production, None, "test");
    let ws = reqwest::Client::new()
        .get(format!("ws://{addr}/"))
        .upgrade()
        .send()
        .await
        .unwrap()
        .into_websocket()
        .await
        .unwrap();
    let (ws, task) = SignalWebSocket::new(ws, "/v1/keepalive".into(), push_service);
    tokio::spawn(task);
    ws
}

/// Lets the socket's task and the server run.
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_check_nobody_answers_closes_the_socket() {
    let ws = connect(false).await;
    // Connected; from here tokio's clock skips ahead when nothing runs.
    tokio::time::pause();
    let started = Instant::now();
    ws.check();
    while !ws.is_closed() {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // At the check's 30 seconds, before the keepalive interval's 55 would
    // notice (the first keepalive went out at the start, unanswered).
    let waited = started.elapsed();
    assert!(waited >= Duration::from_secs(30) && waited < Duration::from_secs(55), "{waited:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn an_answered_check_keeps_the_socket() {
    let ws = connect(true).await;
    ws.check();
    // The answer comes over a real socket, in real time.
    tokio::time::sleep(Duration::from_millis(300)).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(40)).await;
    settle().await;
    assert!(!ws.is_closed());
}
