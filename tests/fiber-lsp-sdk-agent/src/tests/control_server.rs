use std::{future::Future, net::SocketAddr, task::Poll};

use super::*;

async fn request(address: SocketAddr, request: &[u8]) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(request).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    })
    .await
    .expect("control request completes")
}

#[tokio::test]
async fn empty_probe_gets_no_response_or_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut probe = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (stream, _) = listener.accept().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (control_tx, _control_rx) = mpsc::channel(4);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let mut shutdown = Some(shutdown_tx);
    // Half-close so the server sees EOF, while we can still detect an unwanted
    // 404 reply. A full close need not produce a write error on every platform.
    probe.shutdown().await.unwrap();
    let stopped = handle_control_connection(
        stream,
        &dir.path().join("status.json"),
        &control_tx,
        &mut shutdown,
    )
    .await
    .unwrap();
    assert!(!stopped);
    let mut response = Vec::new();
    probe.read_to_end(&mut response).await.unwrap();
    assert!(
        response.is_empty(),
        "an empty probe must not receive an HTTP reply"
    );
    assert!(matches!(
        shutdown_rx.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn reset_probe_does_not_stop_server_but_post_shutdown_does() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let status_file = dir.path().join("status.json");
    std::fs::write(&status_file, br#"{"ready":true}"#).unwrap();
    let (control_tx, _control_rx) = mpsc::channel(4);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let mut shutdown_rx = Some(shutdown_rx);
    let mut server = Box::pin(run_control_server(
        listener,
        status_file,
        control_tx,
        shutdown_tx,
    ));
    let probe = TcpStream::connect(address).await.unwrap();
    // Drive accept/read before resetting the client: a reset still in the
    // listen backlog can otherwise be discarded without exercising the handler.
    std::future::poll_fn(|cx| {
        assert!(server.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    probe.set_zero_linger().unwrap();
    drop(probe);
    let task = tokio::spawn(server);
    let response = request(address, b"GET /status HTTP/1.1\r\n\r\n").await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.ends_with(r#"{"ready":true}"#));
    assert!(matches!(
        shutdown_rx.as_mut().unwrap().try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));

    let response = request(
        address,
        b"POST /shutdown HTTP/1.1\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    tokio::time::timeout(Duration::from_secs(5), wait_for_shutdown(&mut shutdown_rx))
        .await
        .expect("POST /shutdown stops the agent");
    tokio::time::timeout(Duration::from_secs(5), finish_control_server(Some(task)))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn dead_control_task_still_stops_agent() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (control_tx, _control_rx) = mpsc::channel(4);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(run_control_server(
        listener,
        dir.path().join("status.json"),
        control_tx,
        shutdown_tx,
    ));
    task.abort();
    tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_shutdown(&mut Some(shutdown_rx)),
    )
    .await
    .expect("a dead control task stops the agent");
    let error = finish_control_server(Some(task)).await.unwrap_err();
    assert!(error
        .downcast_ref::<tokio::task::JoinError>()
        .unwrap()
        .is_cancelled());
}

#[tokio::test]
async fn control_failure_is_not_reported_as_successful_shutdown() {
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    // Simulate the Result returned by an accept failure without exhausting
    // process-wide file descriptors or invalidating an owned socket.
    let task = tokio::spawn(async move {
        let _shutdown = shutdown_tx;
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("accept control connection")
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_shutdown(&mut Some(shutdown_rx)),
    )
    .await
    .expect("a failed control task wakes the agent");
    let error = finish_control_server(Some(task)).await.unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(format!("{error:#}").contains("accept control connection"));
}
