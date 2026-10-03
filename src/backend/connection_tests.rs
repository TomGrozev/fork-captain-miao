use super::*;
use tokio::io::AsyncReadExt;

#[tokio::test(start_paused = true)]
async fn a_silent_handshake_has_a_deadline() {
    let (client, _peer) = UnixStream::pair().unwrap();
    let (_backend, shared, mut requests) = RemoteBackend::build(
        &Transport::LocalSocket(PathBuf::new()),
        HostId("test".into()),
    );
    let outcome = tokio::time::timeout(
        Duration::from_secs(16),
        serve(
            client,
            None,
            MirrorCells {
                mirror: &shared.mirror,
                presumed_dead: &shared.presumed_dead,
                presumed_attached: &shared.presumed_attached,
                dirty: &shared.dirty,
                mirrored: &shared.mirrored,
                server_version: &shared.server_version,
            },
            &mut requests,
        ),
    )
    .await;
    assert!(
        matches!(outcome, Ok(ServeOutcome::HandshakeFailed(_))),
        "silent handshake must expire: {outcome:?}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn exited_tunnel_is_reaped_while_protocol_stays_connected() {
    let (client, mut peer) = UnixStream::pair().unwrap();
    let mut tunnel = detached("true")
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut output = tunnel.stdout.take().unwrap();
    let pid = tunnel.id().unwrap();
    let (_backend, shared, mut requests) = RemoteBackend::build(
        &Transport::LocalSocket(PathBuf::new()),
        HostId("review".into()),
    );
    write_frame(
        &mut peer,
        &ServerFrame::Welcome {
            server_version: "test".into(),
            protocol: PROTOCOL_VERSION,
            host: "review".into(),
        },
    )
    .await
    .unwrap();
    let connection = serve(
        client,
        Some(tunnel),
        MirrorCells {
            mirror: &shared.mirror,
            presumed_dead: &shared.presumed_dead,
            presumed_attached: &shared.presumed_attached,
            dirty: &shared.dirty,
            mirrored: &shared.mirrored,
            server_version: &shared.server_version,
        },
        &mut requests,
    );
    tokio::pin!(connection);
    let reaped = async {
        output.read_to_end(&mut Vec::new()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
    };
    tokio::select! {
        outcome = &mut connection => panic!("child exit ended the protocol: {outcome:?}"),
        result = reaped => result.expect("connected backend retains an unreaped tunnel process"),
    }
}

#[tokio::test]
async fn dropping_backend_cancels_a_silent_handshake() {
    let dir = scratch_home("review-handshake");
    let sock = dir.join("control.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let backend = RemoteBackend::connect(Transport::LocalSocket(sock), HostId("review".into()));
    let (mut peer, _) = listener.accept().await.unwrap();
    assert!(matches!(
        read_frame::<_, ClientFrame>(&mut peer).await.unwrap(),
        Some(ClientFrame::Hello { .. })
    ));
    drop(backend);
    let closed = tokio::time::timeout(Duration::from_secs(2), peer.read_u8()).await;
    std::fs::remove_dir_all(dir).unwrap();
    assert!(
        matches!(closed, Ok(Err(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof),
        "removed backend keeps its connection alive while awaiting Welcome"
    );
}

#[tokio::test]
async fn expired_queued_mutation_is_not_sent_after_connecting() {
    let (client, mut peer) = UnixStream::pair().unwrap();
    let (backend, shared, mut requests) = RemoteBackend::build(
        &Transport::LocalSocket(PathBuf::new()),
        HostId("review".into()),
    );
    let reply = backend
        .request_within(Duration::from_millis(10), |req_id| {
            ClientFrame::ForgetRecentDir {
                req_id,
                cwd: "~/example".into(),
            }
        })
        .await;
    assert!(
        reply.is_none(),
        "the caller timed out before a connection existed"
    );
    let connection = serve(
        client,
        None,
        MirrorCells {
            mirror: &shared.mirror,
            presumed_dead: &shared.presumed_dead,
            presumed_attached: &shared.presumed_attached,
            dirty: &shared.dirty,
            mirrored: &shared.mirrored,
            server_version: &shared.server_version,
        },
        &mut requests,
    );
    tokio::pin!(connection);
    let peer_check = async {
        assert!(matches!(
            read_frame::<_, ClientFrame>(&mut peer).await.unwrap(),
            Some(ClientFrame::Hello { .. })
        ));
        write_frame(
            &mut peer,
            &ServerFrame::Welcome {
                server_version: "test".into(),
                protocol: PROTOCOL_VERSION,
                host: "review".into(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame::<_, ClientFrame>(&mut peer).await.unwrap(),
            Some(ClientFrame::Subscribe)
        ));
        tokio::time::timeout(
            Duration::from_millis(100),
            read_frame::<_, ClientFrame>(&mut peer),
        )
        .await
    };
    let received = tokio::select! {
        outcome = &mut connection => panic!("unexpected connection outcome: {outcome:?}"),
        received = peer_check => received,
    };
    assert!(
        received.is_err(),
        "expired mutation still sent to the server: {received:?}"
    );
}
