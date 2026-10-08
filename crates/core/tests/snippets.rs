//! Snippet commands round-trip through core and the store (no network).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::StreamExt;
use switchyard_core::db::Engine;
use switchyard_core::store::Snippet;
use switchyard_core::{Command, Core, Event, EventReceiver, ServiceConfig};

async fn next_snippets(rx: &mut EventReceiver) -> Vec<Snippet> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.next().await.expect("event stream ended") {
                Event::Snippets(list) => return list,
                Event::Error { context, message } => panic!("{context}: {message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for snippets")
}

#[tokio::test(flavor = "multi_thread")]
async fn snippet_commands_round_trip() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();
    h.send(Command::LoadSnippets);
    assert!(next_snippets(&mut rx).await.is_empty());

    h.send(Command::SaveSnippet(Snippet::new(
        "Who",
        "who",
        "EXEC sp_who2;",
        Some(Engine::SqlServer),
    )));
    let list = next_snippets(&mut rx).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].engine, Some(Engine::SqlServer));

    h.send(Command::DeleteSnippet {
        id: list[0].id.clone(),
    });
    assert!(next_snippets(&mut rx).await.is_empty());
}
