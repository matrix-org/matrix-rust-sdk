use matrix_sdk::{
    ruma::api::client::room::create_room::v3::Request as CreateRoomRequest, Client, RoomListEntry,
    SlidingSyncBuilder,
};
use matrix_sdk_integration_testing::helpers::get_client_for_user;

#[allow(dead_code)]
async fn setup(name: String) -> anyhow::Result<(Client, SlidingSyncBuilder)> {
    let sliding_sync_proxy_url =
        option_env!("SLIDING_SYNC_PROXY_URL").unwrap_or("http://localhost:8338").to_owned();
    let client = get_client_for_user(name, false).await?;
    let sliding_sync_builder = client
        .sliding_sync()
        .await
        .homeserver(sliding_sync_proxy_url.parse()?)
        .with_common_extensions();
    Ok((client, sliding_sync_builder))
}

#[allow(dead_code)]
async fn random_setup_with_rooms(
    number_of_rooms: usize,
) -> anyhow::Result<(Client, SlidingSyncBuilder)> {
    let namespace = uuid::Uuid::new_v4().to_string();
    let (client, sliding_sync_builder) = setup(namespace.clone()).await?;

    for room_num in 0..number_of_rooms {
        let mut request = CreateRoomRequest::new();
        request.name = Some(format!("{namespace}-{room_num}"));
        let _event_id = client.create_room(request).await?;
    }

    Ok((client, sliding_sync_builder))
}

#[derive(PartialEq, Eq, Debug)]
enum RoomListEntryEasy {
    Empty,
    Invalid,
    Filled,
}

impl From<&RoomListEntry> for RoomListEntryEasy {
    fn from(value: &RoomListEntry) -> Self {
        match value {
            RoomListEntry::Empty => RoomListEntryEasy::Empty,
            RoomListEntry::Invalidated(_) => RoomListEntryEasy::Invalid,
            RoomListEntry::Filled(_) => RoomListEntryEasy::Filled,
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::{pin_mut, stream::StreamExt};
    use matrix_sdk::{
        ruma::events::room::message::RoomMessageEventContent, SlidingSyncMode,
        SlidingSyncViewBuilder,
    };

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn it_works_smoke_test() -> anyhow::Result<()> {
        let (_client, sync_proxy_builder) = setup("odo".to_owned()).await?;
        let sync_proxy = sync_proxy_builder.add_fullsync_view().build().await?;
        let stream = sync_proxy.stream().await?;
        pin_mut!(stream);
        let Some(room_summary ) = stream.next().await else {
            anyhow::bail!("No room summary found, loop ended unsuccessfully");
        };
        let summary = room_summary?;
        assert_eq!(summary.rooms.len(), 0);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn resizing_sliding_window() -> anyhow::Result<()> {
        let (_client, sync_proxy_builder) = random_setup_with_rooms(20).await?;
        let sliding_window_view = SlidingSyncViewBuilder::default()
            .sync_mode(SlidingSyncMode::Selective)
            .set_range(0u32, 10u32)
            .sort(vec!["by_recency".to_string(), "by_name".to_string()])
            .name("sliding")
            .build()?;
        let sync_proxy = sync_proxy_builder.add_view(sliding_window_view).build().await?;
        let Some(view )= sync_proxy.view("sliding") else {
            anyhow::bail!("but we just added that view!");
        };
        let stream = sync_proxy.stream().await?;
        pin_mut!(stream);
        let Some(room_summary ) = stream.next().await else {
            anyhow::bail!("No room summary found, loop ended unsuccessfully");
        };
        let summary = room_summary?;
        // we only heard about the ones we had asked for
        assert_eq!(summary.rooms.len(), 11);
        let collection_simple = view
            .rooms_list
            .lock_ref()
            .iter()
            .map(Into::<RoomListEntryEasy>::into)
            .collect::<Vec<_>>();
        assert_eq!(
            collection_simple,
            [
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
            ]
        );

        let _signal = view.rooms_list.signal_vec_cloned();

        // let's move the window

        view.set_range(1, 10);
        // Ensure 0-0 invalidation ranges work.

        for _n in 0..2 {
            let Some(room_summary ) = stream.next().await else {
                anyhow::bail!("sync has closed unexepectedly");
            };
            let summary = room_summary?;
            // we only heard about the ones we had asked for
            if summary.views.iter().any(|s| s == "sliding") {
                break;
            }
        }

        let collection_simple = view
            .rooms_list
            .lock_ref()
            .iter()
            .map(Into::<RoomListEntryEasy>::into)
            .collect::<Vec<_>>();
        assert_eq!(
            collection_simple,
            [
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
            ]
        );

        view.set_range(5, 10);

        for _n in 0..2 {
            let Some(room_summary ) = stream.next().await else {
                anyhow::bail!("sync has closed unexepectedly");
            };
            let summary = room_summary?;
            // we only heard about the ones we had asked for
            if summary.views.iter().any(|s| s == "sliding") {
                break;
            }
        }

        let collection_simple = view
            .rooms_list
            .lock_ref()
            .iter()
            .map(Into::<RoomListEntryEasy>::into)
            .collect::<Vec<_>>();
        assert_eq!(
            collection_simple,
            [
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
            ]
        );

        // let's move the window

        view.set_range(5, 15);

        for _n in 0..2 {
            let Some(room_summary ) = stream.next().await else {
                anyhow::bail!("sync has closed unexepectedly");
            };
            let summary = room_summary?;
            // we only heard about the ones we had asked for
            if summary.views.iter().any(|s| s == "sliding") {
                break;
            }
        }

        let collection_simple = view
            .rooms_list
            .lock_ref()
            .iter()
            .map(Into::<RoomListEntryEasy>::into)
            .collect::<Vec<_>>();
        assert_eq!(
            collection_simple,
            [
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
            ]
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn moving_out_of_sliding_window() -> anyhow::Result<()> {
        let (client, sync_proxy_builder) = random_setup_with_rooms(20).await?;
        let sliding_window_view = SlidingSyncViewBuilder::default()
            .sync_mode(SlidingSyncMode::Selective)
            .set_range(1u32, 10u32)
            .sort(vec!["by_recency".to_string(), "by_name".to_string()])
            .name("sliding")
            .build()?;
        let sync_proxy = sync_proxy_builder.add_view(sliding_window_view).build().await?;
        let Some(view )= sync_proxy.view("sliding") else {
            anyhow::bail!("but we just added that view!");
        };
        let stream = sync_proxy.stream().await?;
        pin_mut!(stream);
        let Some(room_summary ) = stream.next().await else {
            anyhow::bail!("No room summary found, loop ended unsuccessfully");
        };
        let summary = room_summary?;
        // we only heard about the ones we had asked for
        assert_eq!(summary.rooms.len(), 10);
        let collection_simple = view
            .rooms_list
            .lock_ref()
            .iter()
            .map(Into::<RoomListEntryEasy>::into)
            .collect::<Vec<_>>();
        assert_eq!(
            collection_simple,
            [
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
            ]
        );

        let _signal = view.rooms_list.signal_vec_cloned();

        // let's move the window

        view.set_range(0, 10);

        for _n in 0..2 {
            let Some(room_summary ) = stream.next().await else {
                anyhow::bail!("sync has closed unexepectedly");
            };
            let summary = room_summary?;
            // we only heard about the ones we had asked for
            if summary.views.iter().any(|s| s == "sliding") {
                break;
            }
        }

        let collection_simple = view
            .rooms_list
            .lock_ref()
            .iter()
            .map(Into::<RoomListEntryEasy>::into)
            .collect::<Vec<_>>();
        assert_eq!(
            collection_simple,
            [
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
            ]
        );

        // let's move the window again

        view.set_range(2, 12);

        for _n in 0..2 {
            let Some(room_summary ) = stream.next().await else {
                anyhow::bail!("sync has closed unexepectedly");
            };
            let summary = room_summary?;
            // we only heard about the ones we had asked for
            if summary.views.iter().any(|s| s == "sliding") {
                break;
            }
        }

        let collection_simple = view
            .rooms_list
            .lock_ref()
            .iter()
            .map(Into::<RoomListEntryEasy>::into)
            .collect::<Vec<_>>();
        assert_eq!(
            collection_simple,
            [
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
            ]
        );

        // now we "move" the room of pos 3 to pos 0;
        // this is a bordering case

        let Some(RoomListEntry::Filled(room_id)) = view
            .rooms_list
            .lock_ref()
            .iter().nth(3).map(Clone::clone) else
        {
            anyhow::bail!("2nd room has moved? how?");
        };

        let Some(room) = client.get_joined_room(&room_id) else {
            anyhow::bail!("No joined room {room_id}");
        };

        let content = RoomMessageEventContent::text_plain("Hello world");

        room.send(content, None).await?; // this should put our room up to the most recent

        for _n in 0..2 {
            let Some(room_summary ) = stream.next().await else {
                anyhow::bail!("sync has closed unexepectedly");
            };
            let summary = room_summary?;
            // we only heard about the ones we had asked for
            if summary.views.iter().any(|s| s == "sliding") {
                break;
            }
        }

        let collection_simple = view
            .rooms_list
            .lock_ref()
            .iter()
            .map(Into::<RoomListEntryEasy>::into)
            .collect::<Vec<_>>();
        assert_eq!(
            collection_simple,
            [
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
            ]
        );

        // items has moved, thus we shouldn't find it where it was
        assert!(
            view.rooms_list.lock_ref().iter().nth(3).unwrap().as_room_id().unwrap() != &room_id
        );

        // let's move the window again

        view.set_range(0, 10);

        for _n in 0..2 {
            let Some(room_summary ) = stream.next().await else {
                anyhow::bail!("sync has closed unexepectedly");
            };
            let summary = room_summary?;
            // we only heard about the ones we had asked for
            if summary.views.iter().any(|s| s == "sliding") {
                break;
            }
        }

        let collection_simple = view
            .rooms_list
            .lock_ref()
            .iter()
            .map(Into::<RoomListEntryEasy>::into)
            .collect::<Vec<_>>();
        assert_eq!(
            collection_simple,
            [
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Filled,
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Invalid,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
                RoomListEntryEasy::Empty,
            ]
        );

        // and check that our room move has been accepted properly, too.
        assert_eq!(
            view.rooms_list.lock_ref().iter().next().unwrap().as_room_id().unwrap(),
            &room_id
        );

        Ok(())
    }
}
