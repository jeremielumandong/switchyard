//! Favorite (pin) commands round-trip through core and the store (no network).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::StreamExt;
use switchyard_core::db::ObjectKind;
use switchyard_core::store::{Favorite, ProfileId};
use switchyard_core::{Command, Core, Event, EventReceiver, ServiceConfig};

async fn next_favorites(rx: &mut EventReceiver) -> Vec<Favorite> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.next().await.expect("event stream ended") {
                Event::Favorites(list) => return list,
                Event::Error { context, message } => panic!("{context}: {message}"),
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for favorites")
}

#[tokio::test(flavor = "multi_thread")]
async fn favorite_commands_round_trip() {
    let (core, mut rx) = Core::start(ServiceConfig::in_memory()).unwrap();
    let h = core.handle();
    h.send(Command::LoadFavorites);
    assert!(next_favorites(&mut rx).await.is_empty());

    let c = ProfileId("c".into());
    h.send(Command::AddFavorite(Favorite::object(
        c.clone(),
        "shop",
        "public",
        "orders",
        ObjectKind::Table,
    )));
    assert_eq!(next_favorites(&mut rx).await.len(), 1);
    h.send(Command::AddFavorite(Favorite::schema(c, "shop", "sales")));
    let list = next_favorites(&mut rx).await;
    assert_eq!(list.len(), 2);
    assert_eq!(list[1].kind, None);

    h.send(Command::ReorderFavorites {
        ids: vec![list[1].id, list[0].id],
    });
    let reordered = next_favorites(&mut rx).await;
    assert_eq!(reordered[0].id, list[1].id);

    h.send(Command::RemoveFavorite { id: list[0].id });
    let left = next_favorites(&mut rx).await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].schema, "sales");
}
