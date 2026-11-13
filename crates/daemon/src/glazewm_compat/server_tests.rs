use super::*;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Error};

type Client = tokio_tungstenite::WebSocketStream<TcpStream>;

async fn open(addr: SocketAddr) -> Client {
    let stream = TcpStream::connect(addr).await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::client_async(format!("ws://{addr}/"), stream),
    )
    .await
    .unwrap()
    .unwrap()
    .0
}

async fn receive(socket: &mut Client) -> Value {
    let text = tokio::time::timeout(Duration::from_secs(3), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .into_text()
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn loopback_same_user_origin_and_frame_limit() {
    let listener = bind(0).unwrap();
    let addr = listener.local_addr().unwrap();
    assert_eq!(addr.ip(), Ipv4Addr::LOCALHOST);
    assert!(bind(addr.port()).is_err());
    let (events, _receiver) = mpsc::channel(16);
    let task = tokio::spawn(run(listener, events));
    let mut socket = open(addr).await;
    socket
        .send(Message::Text("query windows".into()))
        .await
        .unwrap();
    let reply = receive(&mut socket).await;
    assert_eq!(reply["success"], false);
    assert_eq!(reply["clientMessage"], "query windows");

    let stream = TcpStream::connect(addr).await.unwrap();
    let mut request = format!("ws://{addr}/").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("origin", "https://example.invalid".parse().unwrap());
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::client_async(request, stream),
    )
    .await
    .unwrap();
    assert!(
        matches!(result, Err(Error::Http(response)) if response.status() == StatusCode::FORBIDDEN)
    );

    socket
        .send(Message::Text("x".repeat(LIMIT + 1).into()))
        .await
        .unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(3), socket.next())
        .await
        .unwrap();
    assert!(!matches!(closed, Some(Ok(Message::Text(_)))));
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn enable_disable_rebind_and_collision_do_not_retry() {
    let first = bind(0).unwrap();
    let first_addr = first.local_addr().unwrap();
    let second = bind(0).unwrap();
    let second_addr = second.local_addr().unwrap();
    drop(first);
    drop(second);
    let (events, _receiver) = mpsc::channel(16);
    let mut server = Server::default();
    let mut config = GlazewmCompatConfig {
        enabled: false,
        port: first_addr.port(),
    };
    server.configure(&config, events.clone()).await;
    assert!(TcpStream::connect(first_addr).await.is_err());
    config.enabled = true;
    server.configure(&config, events.clone()).await;
    assert!(TcpStream::connect(first_addr).await.is_ok());
    config.port = second_addr.port();
    server.configure(&config, events.clone()).await;
    assert!(TcpStream::connect(first_addr).await.is_err());
    assert!(TcpStream::connect(second_addr).await.is_ok());
    config.enabled = false;
    server.configure(&config, events.clone()).await;
    assert!(TcpStream::connect(second_addr).await.is_err());

    let occupied = bind(first_addr.port()).unwrap();
    config.enabled = true;
    config.port = first_addr.port();
    server.configure(&config, events.clone()).await;
    assert!(server.task.is_none());
    drop(occupied);
    server.configure(&config, events).await;
    assert!(server.task.is_none());
    assert!(TcpStream::connect(first_addr).await.is_err());
}

#[tokio::test]
async fn unchanged_config_restarts_finished_listener() {
    let listener = bind(0).unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let (events, _receiver) = mpsc::channel(16);
    let mut server = Server::default();
    let config = GlazewmCompatConfig {
        enabled: true,
        port: addr.port(),
    };
    server.configure(&config, events.clone()).await;
    assert!(TcpStream::connect(addr).await.is_ok());
    let task = server.task.as_ref().unwrap();
    task.abort();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.configure(&config, events).await;
    assert!(TcpStream::connect(addr).await.is_ok());
}

async fn send_text(client: &mut Client, message: &str) -> Value {
    client.send(Message::Text(message.into())).await.unwrap();
    receive(client).await
}

#[tokio::test]
async fn connected_clients_share_snapshot_and_receive_only_subscribed_revisions() {
    let monitor = leopardwm_platform_win32::enumerate_monitors()
        .unwrap()
        .remove(0);
    let snapshot = WorkspaceStateSnapshot {
        session_id: "test".into(),
        revision: 1,
        focused_monitor_device_name: Some(monitor.device_name.clone()),
        records: vec![
            leopardwm_ipc::WorkspaceStateRecord::Monitor {
                monitor_device_name: monitor.device_name.clone(),
                monitor_id: monitor.id as i64,
                active_workspace_index: 0,
            },
            leopardwm_ipc::WorkspaceStateRecord::Workspace {
                monitor_device_name: monitor.device_name,
                workspace_index: 0,
                name: None,
                rows: None,
            },
        ],
    };
    let listener = bind(0).unwrap();
    let addr = listener.local_addr().unwrap();
    let (events, mut event_rx) = mpsc::channel(16);
    let task = tokio::spawn(run(listener, events));
    let mut first = open(addr).await;
    let Some(DaemonEvent::IpcSubscribe { events, responder }) =
        tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
            .await
            .unwrap()
    else {
        panic!("expected one workspace subscription")
    };
    assert_eq!(events, [EventKind::WorkspaceState].into_iter().collect());
    let (updates, receiver) = broadcast::channel(32);
    responder
        .send(crate::events::SubscribeStartup {
            ack: IpcResponse::Subscribed { events },
            snapshot: snapshot.events().unwrap(),
            receiver,
        })
        .ok()
        .unwrap();
    let mut second = open(addr).await;
    let mut third = open(addr).await;
    assert_eq!(
        send_text(&mut first, "query monitors").await["success"],
        true
    );
    let subscription = send_text(&mut first, "sub -e workspace_updated").await;
    assert_eq!(subscription["success"], true);
    let subscription_id = subscription["data"]["subscriptionId"].as_str().unwrap();
    let unsubscribe = format!("unsub --id {subscription_id}");
    assert_eq!(
        send_text(&mut second, "sub --events all").await["success"],
        true
    );
    assert_eq!(
        send_text(&mut third, "query monitors").await["success"],
        true
    );
    assert_eq!(updates.receiver_count(), 1);
    for event in snapshot.events().unwrap() {
        updates.send(event).unwrap();
    }
    let mut changed = snapshot;
    changed.revision += 1;
    for event in changed.events().unwrap() {
        updates.send(event).unwrap();
    }
    for client in [&mut first, &mut second] {
        let event = receive(client).await;
        assert_eq!(event["messageType"], "event_subscription");
        assert_eq!(event["data"]["eventType"], "workspace_updated");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), client.next())
                .await
                .is_err()
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), third.next())
            .await
            .is_err()
    );
    assert_eq!(send_text(&mut first, &unsubscribe).await["success"], true);
    changed.revision += 1;
    changed.focused_monitor_device_name = None;
    for event in changed.events().unwrap() {
        updates.send(event).unwrap();
    }
    let event = receive(&mut second).await;
    assert_eq!(event["data"]["eventType"], "workspace_updated");
    assert!(event["data"]["updatedWorkspace"].is_null());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), first.next())
            .await
            .is_err()
    );
    drop((first, second, third));
    tokio::time::timeout(Duration::from_secs(3), async {
        while updates.receiver_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn lagged_feed_resubscribes_and_replaces_snapshot() {
    let (events, mut event_rx) = mpsc::channel(8);
    let (snapshots, mut current) = watch::channel(None);
    let (changes, mut updates) = broadcast::channel(8);
    let task = tokio::spawn(snapshot_feed(events, snapshots, changes));
    let Some(DaemonEvent::IpcSubscribe { events, responder }) = event_rx.recv().await else {
        panic!("expected subscription")
    };
    let (source, receiver) = broadcast::channel(2);
    let snapshot = WorkspaceStateSnapshot {
        session_id: "test".into(),
        revision: 1,
        focused_monitor_device_name: None,
        records: vec![],
    };
    responder
        .send(crate::events::SubscribeStartup {
            ack: IpcResponse::Subscribed { events },
            snapshot: snapshot.events().unwrap(),
            receiver,
        })
        .ok()
        .unwrap();
    current.changed().await.unwrap();
    for _ in 0..4 {
        source
            .send(IpcEvent::WorkspaceSnapshotEnd { revision: 1 })
            .unwrap();
    }
    let Some(DaemonEvent::IpcSubscribe { events, responder }) =
        tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
            .await
            .unwrap()
    else {
        panic!("expected resubscription")
    };
    assert!(
        current.borrow().is_none(),
        "lag must invalidate the stale snapshot"
    );
    let mut replacement = snapshot;
    replacement.revision = 2;
    responder
        .send(crate::events::SubscribeStartup {
            ack: IpcResponse::Subscribed { events },
            snapshot: replacement.events().unwrap(),
            receiver: source.subscribe(),
        })
        .ok()
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), updates.recv())
            .await
            .unwrap()
            .unwrap()
            .revision,
        2
    );
    assert_eq!(current.borrow().as_ref().unwrap().revision, 2);
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn timed_out_feed_disconnects_clients_and_recovers_after_late_reply() {
    let listener = bind(0).unwrap();
    let addr = listener.local_addr().unwrap();
    let (events, mut event_rx) = mpsc::channel(16);
    let task = tokio::spawn(run(listener, events));
    let mut first = open(addr).await;
    let Some(DaemonEvent::IpcSubscribe { events, responder }) = event_rx.recv().await else {
        panic!("expected subscription")
    };
    let (source, receiver) = broadcast::channel(8);
    let snapshot = WorkspaceStateSnapshot {
        session_id: "test".into(),
        revision: 1,
        focused_monitor_device_name: None,
        records: vec![],
    };
    let closed = tokio::time::timeout(Duration::from_secs(13), first.next())
        .await
        .expect("timed-out feed must disconnect its client");
    assert!(matches!(
        closed,
        None | Some(Err(_)) | Some(Ok(Message::Close(_)))
    ));
    assert!(
        responder
            .send(crate::events::SubscribeStartup {
                ack: IpcResponse::Subscribed { events },
                snapshot: snapshot.events().unwrap(),
                receiver,
            })
            .is_err(),
        "the delayed subscriber answers after the feed timeout"
    );
    let mut second = open(addr).await;
    let Some(DaemonEvent::IpcSubscribe { events, responder }) =
        tokio::time::timeout(Duration::from_secs(3), event_rx.recv())
            .await
            .unwrap()
    else {
        panic!("expected fresh subscription after reconnect")
    };
    responder
        .send(crate::events::SubscribeStartup {
            ack: IpcResponse::Subscribed { events },
            snapshot: snapshot.events().unwrap(),
            receiver: source.subscribe(),
        })
        .ok()
        .unwrap();
    assert_eq!(
        send_text(&mut second, "query monitors").await["success"],
        true
    );
    drop(second);
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn startup_snapshot_error_disconnects_without_repeated_subscribes() {
    let listener = bind(0).unwrap();
    let addr = listener.local_addr().unwrap();
    let (events, mut event_rx) = mpsc::channel(8);
    let task = tokio::spawn(run(listener, events));
    let mut client = open(addr).await;
    let (source, _) = broadcast::channel(8);
    let mut subscribes = 0;
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    let Some(DaemonEvent::IpcSubscribe { events, responder }) = event else {
                        panic!("expected subscription")
                    };
                    subscribes += 1;
                    responder.send(crate::events::SubscribeStartup {
                        ack: IpcResponse::Subscribed { events },
                        snapshot: vec![IpcEvent::WorkspaceSnapshotError { message: "snapshot too large".into() }],
                        receiver: source.subscribe(),
                    }).ok().unwrap();
                    assert_eq!(subscribes, 1, "startup errors must not cause immediate resubscription");
                }
                message = client.next() => break message,
            }
        }
    }).await.expect("startup snapshot error must disconnect the client");
    assert!(matches!(
        closed,
        None | Some(Err(_)) | Some(Ok(Message::Close(_)))
    ));
    assert_eq!(subscribes, 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), event_rx.recv())
            .await
            .is_err()
    );
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn connection_limit_rejects_ninth_peer() {
    let listener = bind(0).unwrap();
    let addr = listener.local_addr().unwrap();
    let (events, _event_rx) = mpsc::channel(16);
    let task = tokio::spawn(run(listener, events));
    let mut clients = Vec::new();
    for _ in 0..8 {
        clients.push(open(addr).await);
    }
    let stream = TcpStream::connect(addr).await.unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::client_async(format!("ws://{addr}/"), stream),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    drop(clients);
    task.abort();
    let _ = task.await;
}
