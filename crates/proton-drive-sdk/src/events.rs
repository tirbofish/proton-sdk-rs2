use crate::api::events::{VolumeEventType, VolumeEventsResponse};
use crate::client::ProtonDriveClient;
use crate::node::NodeUid;
use crate::volume::VolumeId;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const OWN_VOLUME_POLL: Duration = Duration::from_secs(30);
const CORE_POLL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SdkEvent {
    TransfersPaused,
    TransfersResumed,
    RequestsThrottled,
    RequestsUnthrottled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveEventType {
    NodeCreated,
    NodeUpdated,
    NodeDeleted,
    SharedWithMeUpdated,
    TreeRefresh,
}

#[derive(Debug, Clone)]
pub struct DriveEvent {
    pub event_type: DriveEventType,
    pub event_id: String,
    pub node_uid: Option<NodeUid>,
    pub parent_node_uid: Option<NodeUid>,
    pub is_trashed: bool,
    pub is_shared: bool,
    pub tree_event_scope_id: String,
}

#[derive(Default)]
pub struct SdkEvents {
    listeners: Mutex<Vec<(u64, SdkEvent, Arc<dyn Fn() + Send + Sync>)>>,
    next_listener_id: AtomicU64,
}

impl SdkEvents {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_listener(
        self: &Arc<Self>,
        event: SdkEvent,
        callback: Arc<dyn Fn() + Send + Sync>,
    ) -> impl Fn() + Send + Sync + 'static {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners
            .lock()
            .expect("sdk event listeners")
            .push((id, event, callback));
        let events = Arc::downgrade(self);
        move || {
            if let Some(events) = events.upgrade() {
                events
                    .listeners
                    .lock()
                    .expect("sdk event listeners")
                    .retain(|(listener_id, _, _)| *listener_id != id);
            }
        }
    }

    pub fn transfers_paused(&self) {
        self.emit(SdkEvent::TransfersPaused);
    }

    pub fn transfers_resumed(&self) {
        self.emit(SdkEvent::TransfersResumed);
    }

    pub fn requests_throttled(&self) {
        self.emit(SdkEvent::RequestsThrottled);
    }

    pub fn requests_unthrottled(&self) {
        self.emit(SdkEvent::RequestsUnthrottled);
    }

    fn emit(&self, event: SdkEvent) {
        let listeners = self.listeners.lock().expect("sdk event listeners");
        for (_, kind, cb) in listeners.iter() {
            if *kind == event {
                cb();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn unsubscribe_removes_listener() {
        let events = Arc::new(SdkEvents::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let unsubscribe = events.add_listener(
            SdkEvent::TransfersPaused,
            Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        );
        events.transfers_paused();
        unsubscribe();
        events.transfers_paused();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn emits_only_to_matching_listeners() {
        let events = Arc::new(SdkEvents::new());
        let throttled = Arc::new(AtomicUsize::new(0));
        let unthrottled = Arc::new(AtomicUsize::new(0));

        let throttled_counter = throttled.clone();
        let _keep_throttled = events.add_listener(
            SdkEvent::RequestsThrottled,
            Arc::new(move || {
                throttled_counter.fetch_add(1, Ordering::Relaxed);
            }),
        );
        let unthrottled_counter = unthrottled.clone();
        let _keep_unthrottled = events.add_listener(
            SdkEvent::RequestsUnthrottled,
            Arc::new(move || {
                unthrottled_counter.fetch_add(1, Ordering::Relaxed);
            }),
        );

        events.requests_throttled();
        assert_eq!(throttled.load(Ordering::Relaxed), 1);
        assert_eq!(unthrottled.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn emits_to_every_matching_listener() {
        let events = Arc::new(SdkEvents::new());
        let calls = Arc::new(AtomicUsize::new(0));

        for _ in 0..2 {
            let counter = calls.clone();
            let _ = events.add_listener(
                SdkEvent::TransfersResumed,
                Arc::new(move || {
                    counter.fetch_add(1, Ordering::Relaxed);
                }),
            );
        }

        events.transfers_resumed();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn dispatches_create_update_and_delete_events() {
        use crate::api::events::{VolumeEventDto, VolumeEventLinkDto};
        use crate::api::{ApiResponse, ResponseCode};
        use crate::links::LinkId;
        use parking_lot::Mutex;

        let response = VolumeEventsResponse {
            base: ApiResponse {
                code: ResponseCode::SUCCESS,
                error_message: None,
            },
            event_id: "latest".into(),
            more: false,
            refresh: false,
            events: [1, 2, 3, 0]
                .into_iter()
                .enumerate()
                .map(|(index, event_type)| VolumeEventDto {
                    event_id: index.to_string(),
                    event_type,
                    link: VolumeEventLinkDto {
                        link_id: LinkId::new(format!("link-{index}")),
                        parent_link_id: Some(LinkId::new("parent".into())),
                        is_shared: true,
                        is_trashed: event_type == 0,
                    },
                })
                .collect(),
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let callback: Arc<dyn Fn(DriveEvent) + Send + Sync> =
            Arc::new(move |event| captured.lock().push(event));

        dispatch_volume_events(&VolumeId::new("volume".into()), &response, &callback);

        let events = events.lock();
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type)
                .collect::<Vec<_>>(),
            vec![
                DriveEventType::NodeCreated,
                DriveEventType::NodeUpdated,
                DriveEventType::NodeUpdated,
                DriveEventType::NodeDeleted,
            ]
        );
        assert_eq!(events[0].node_uid.as_ref().unwrap().raw(), "volume~link-0");
        assert_eq!(
            events[0].parent_node_uid.as_ref().unwrap().raw(),
            "volume~parent"
        );
        assert!(events[0].is_shared);
        assert!(events[3].is_trashed);
    }

    #[test]
    fn ignores_unknown_volume_event_types() {
        use crate::api::events::{VolumeEventDto, VolumeEventLinkDto};
        use crate::api::{ApiResponse, ResponseCode};
        use crate::links::LinkId;

        let response = VolumeEventsResponse {
            base: ApiResponse {
                code: ResponseCode::SUCCESS,
                error_message: None,
            },
            event_id: "latest".into(),
            more: false,
            refresh: false,
            events: vec![VolumeEventDto {
                event_id: "event".into(),
                event_type: 99,
                link: VolumeEventLinkDto {
                    link_id: LinkId::new("link".into()),
                    parent_link_id: None,
                    is_shared: false,
                    is_trashed: false,
                },
            }],
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let callback: Arc<dyn Fn(DriveEvent) + Send + Sync> = Arc::new(move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
        });

        dispatch_volume_events(&VolumeId::new("volume".into()), &response, &callback);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
}

pub struct EventSubscription {
    stop_tx: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl EventSubscription {
    pub fn dispose(self) {
        let _ = self.stop_tx.send(true);
        self.task.abort();
    }

    pub fn abort(&self) {
        let _ = self.stop_tx.send(true);
        self.task.abort();
    }
}

pub async fn subscribe_to_tree_events(
    client: ProtonDriveClient,
    volume_id: VolumeId,
    callback: Arc<dyn Fn(DriveEvent) + Send + Sync>,
) -> anyhow::Result<EventSubscription> {
    let mut event_id = client.get_volume_latest_event_id(volume_id.clone()).await?;
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        loop {
            if *stop_rx.borrow() {
                break;
            }
            match client
                .poll_volume_events(volume_id.clone(), &event_id)
                .await
            {
                Ok(response) => {
                    dispatch_volume_events(&volume_id, &response, &callback);
                    event_id = response.event_id;
                    if response.refresh {
                        callback(DriveEvent {
                            event_type: DriveEventType::TreeRefresh,
                            event_id: event_id.clone(),
                            node_uid: None,
                            parent_node_uid: None,
                            is_trashed: false,
                            is_shared: false,
                            tree_event_scope_id: volume_id.raw().to_string(),
                        });
                    }
                    if !response.more {
                        tokio::select! {
                            _ = tokio::time::sleep(OWN_VOLUME_POLL) => {}
                            _ = stop_rx.changed() => {}
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "volume event poll failed");
                    tokio::time::sleep(OWN_VOLUME_POLL).await;
                }
            }
        }
    });
    Ok(EventSubscription { stop_tx, task })
}

pub async fn subscribe_to_drive_events(
    client: ProtonDriveClient,
    callback: Arc<dyn Fn(DriveEvent) + Send + Sync>,
) -> anyhow::Result<EventSubscription> {
    let my_files = client.get_my_files_folder().await?;
    let own_volume = my_files.base.uid.volume_id.clone();
    let mut volume_event_id = client
        .get_volume_latest_event_id(own_volume.clone())
        .await?;
    let mut core_event_id = client.get_core_latest_event_id().await?;
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let cb = callback.clone();
    let task = tokio::spawn(async move {
        let mut core_tick = tokio::time::interval(CORE_POLL);
        let mut volume_tick = tokio::time::interval(OWN_VOLUME_POLL);
        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        break;
                    }
                }
                _ = volume_tick.tick() => {
                    if let Ok(response) = client.poll_volume_events(own_volume.clone(), &volume_event_id).await {
                        dispatch_volume_events(&own_volume, &response, &cb);
                        volume_event_id = response.event_id;
                    }
                }
                _ = core_tick.tick() => {
                    if let Ok(response) = client.poll_core_events(&core_event_id).await {
                        core_event_id = response.event_id.clone();
                        if response.drive_share_refresh.is_some() {
                            cb(DriveEvent {
                                event_type: DriveEventType::SharedWithMeUpdated,
                                event_id: core_event_id.clone(),
                                node_uid: None,
                                parent_node_uid: None,
                                is_trashed: false,
                                is_shared: false,
                                tree_event_scope_id: "core".into(),
                            });
                        }
                    }
                }
            }
        }
    });
    Ok(EventSubscription { stop_tx, task })
}

fn dispatch_volume_events(
    volume_id: &VolumeId,
    response: &VolumeEventsResponse,
    callback: &Arc<dyn Fn(DriveEvent) + Send + Sync>,
) {
    for event in &response.events {
        let event_type = match event.event_type() {
            Some(VolumeEventType::Create) => DriveEventType::NodeCreated,
            Some(VolumeEventType::UpdateMetadata | VolumeEventType::UpdateContent) => {
                DriveEventType::NodeUpdated
            }
            Some(VolumeEventType::Delete) => DriveEventType::NodeDeleted,
            None => continue,
        };
        callback(DriveEvent {
            event_type,
            event_id: event.event_id.clone(),
            node_uid: Some(NodeUid::new(volume_id.clone(), event.link.link_id.clone())),
            parent_node_uid: event
                .link
                .parent_link_id
                .clone()
                .map(|id| NodeUid::new(volume_id.clone(), id)),
            is_trashed: event.link.is_trashed,
            is_shared: event.link.is_shared,
            tree_event_scope_id: volume_id.raw().to_string(),
        });
    }
}
