use std::{sync::Arc, time::Duration};

use biscuit_auth::{
    macros::{biscuit, block},
    KeyPair,
};
use fiber_json_types::{SubscriptionEvent, SubscriptionTopic};
use fiber_types::Hash256;
use hyper::{
    header::{HeaderValue, AUTHORIZATION},
    HeaderMap,
};
use jsonrpsee::{
    core::client::{ClientT, Subscription, SubscriptionClientT},
    rpc_params,
    server::ServerHandle,
    ws_client::{WsClient, WsClientBuilder},
    RpcModule,
};

use crate::{
    fiber::channel::{ChannelActorState, ChannelActorStateStore},
    rpc::{biscuit::BiscuitAuth, middleware::RpcAuthorization},
    store::{open_store, sample::StoreSample, NodeNamespace, Store},
};

use super::{SubscriptionRpcServer, SubscriptionRpcServerImpl};

struct Fixture {
    events: crate::signing::SigningEvents,
    store: Store,
    root: KeyPair,
    address: std::net::SocketAddr,
    handle: ServerHandle,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new(authenticated: bool, channels: bool, watchtower: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = open_store(dir.path()).unwrap();
        let events = crate::signing::SigningEvents::default();
        let root = KeyPair::new();
        let auth =
            authenticated.then(|| BiscuitAuth::from_pubkey(root.public().to_string()).unwrap());
        let mut module = RpcModule::new(());
        module
            .merge(
                SubscriptionRpcServerImpl::new(
                    events.clone(),
                    store.clone(),
                    None,
                    channels,
                    watchtower,
                )
                .into_rpc(),
            )
            .unwrap();
        let (handle, address) =
            crate::rpc::server::start_server("127.0.0.1:0", auth, module, false, vec![])
                .await
                .unwrap();
        Self {
            events,
            store,
            root,
            address,
            handle,
            _dir: dir,
        }
    }

    async fn client(&self, token: Option<&str>) -> WsClient {
        let mut headers = HeaderMap::new();
        if let Some(token) = token {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
            );
        }
        WsClientBuilder::default()
            .set_headers(headers)
            .build(format!("ws://{}", self.address))
            .await
            .unwrap()
    }

    fn token(&self, facts: &str) -> String {
        biscuit_auth::Biscuit::builder()
            .code(facts)
            .unwrap()
            .build(&self.root)
            .unwrap()
            .to_base64()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.handle.stop();
    }
}

async fn subscribe(client: &WsClient, topic: SubscriptionTopic) -> Subscription<SubscriptionEvent> {
    client
        .subscribe("subscribe", rpc_params![topic], "unsubscribe")
        .await
        .unwrap()
}

async fn next(subscription: &mut Subscription<SubscriptionEvent>) -> SubscriptionEvent {
    tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn subscription_host_scope_ignores_attenuation_and_tenant_writes() {
    let fixture = Fixture::new(true, true, false).await;
    let token = biscuit!(r#"write("subscriptions"); read("channels");"#)
        .build(&fixture.root)
        .unwrap()
        .append(block!(r#"tenant("alice"); node("0x0102");"#))
        .unwrap()
        .to_base64()
        .unwrap();
    let client = fixture.client(Some(&token)).await;
    let mut first = subscribe(&client, SubscriptionTopic::ChannelSigning).await;
    let mut second = subscribe(&client, SubscriptionTopic::ChannelSigning).await;
    let tenant = fixture
        .store
        .namespaced(NodeNamespace::hosted_tenant("alice"));
    let state = ChannelActorState::samples(42).remove(0);
    let id = state.get_id();
    {
        let publisher = fixture.events.channels(Some("alice".into()));
        let update = publisher.prepare(&state);
        tenant.insert_channel_actor_state(state.clone());
        update.publish();
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), first.next())
            .await
            .is_err()
    );
    {
        let publisher = fixture.events.channels(None);
        let update = publisher.prepare(&state);
        fixture.store.insert_channel_actor_state(state);
        update.publish();
    };
    for subscription in [&mut first, &mut second] {
        let SubscriptionEvent::ChannelSigning(result) = next(subscription).await else {
            panic!("channel event");
        };
        assert_eq!(result.channel_id, id.into());
    }
    {
        let publisher = fixture.events.channels(None);
        let update = publisher.prepare_removed(id);
        fixture.store.delete_channel_actor_state(&id);
        update.publish();
    };
    for subscription in [&mut first, &mut second] {
        assert!(
            matches!(next(subscription).await, SubscriptionEvent::Removed { channel_id } if channel_id == id.into())
        );
    }
    first.unsubscribe().await.unwrap();
    second.unsubscribe().await.unwrap();
}

#[tokio::test]
async fn subscription_requires_control_and_topic_permissions() {
    let fixture = Fixture::new(true, true, false).await;
    for facts in [
        r#"read("channels");"#,
        r#"read("subscriptions"); read("channels");"#,
    ] {
        let token = fixture.token(facts);
        let client = fixture.client(Some(&token)).await;
        let error = client
            .subscribe::<SubscriptionEvent, _>(
                "subscribe",
                rpc_params!["channel_signing"],
                "unsubscribe",
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, jsonrpsee::core::ClientError::Call(error) if error.code() == -32999)
        );
        let error = client
            .request::<bool, _>("unsubscribe", rpc_params!["unknown"])
            .await
            .unwrap_err();
        assert!(
            matches!(error, jsonrpsee::core::ClientError::Call(error) if error.code() == -32999)
        );
    }
    let token = fixture.token(r#"write("subscriptions");"#);
    let client = fixture.client(Some(&token)).await;
    let error = client
        .subscribe::<SubscriptionEvent, _>(
            "subscribe",
            rpc_params!["channel_signing"],
            "unsubscribe",
        )
        .await
        .unwrap_err();
    assert!(matches!(error, jsonrpsee::core::ClientError::Call(error)
        if error.code() == -32001 && error.message() == "Unauthorized"));
    assert!(!client
        .request::<bool, _>("unsubscribe", rpc_params!["unknown"])
        .await
        .unwrap());
}

#[tokio::test]
async fn subscription_rejects_unreadable_topics_invalid_filters_and_spoofed_identity() {
    let fixture = Fixture::new(true, true, false).await;
    let token = fixture.token(r#"write("subscriptions"); write("channels");"#);
    let client = fixture.client(Some(&token)).await;
    assert!(client
        .subscribe::<SubscriptionEvent, _>(
            "subscribe",
            rpc_params!["channel_signing"],
            "unsubscribe"
        )
        .await
        .is_err());
    let token = fixture.token(r#"write("subscriptions"); read("channels");"#);
    let client = fixture.client(Some(&token)).await;
    for params in [
        serde_json::json!(["channel_signing", {"tenant_id":"alice"}]),
        serde_json::json!(["channel_signing", {"include_snapshot": true}]),
        serde_json::json!(["channel_signing", {"node_id":"host"}]),
        serde_json::json!(["channel_signing", {"channel_ids":[format!("{:#x}", Hash256::from([1;32]))]}]),
        serde_json::json!(["watchtower_signing"]),
        serde_json::json!(["unknown"]),
    ] {
        let mut arguments = jsonrpsee::core::params::ArrayParams::new();
        for argument in params.as_array().unwrap() {
            arguments.insert(argument).unwrap();
        }
        assert!(client
            .subscribe::<SubscriptionEvent, _>("subscribe", arguments, "unsubscribe")
            .await
            .is_err());
    }
    let subscription = subscribe(&client, SubscriptionTopic::ChannelSigning).await;
    subscription.unsubscribe().await.unwrap();
}

#[tokio::test]
async fn subscription_keeps_existing_scope_but_rejects_expired_requests() {
    let fixture = Fixture::new(true, true, false).await;
    let expiry = std::time::SystemTime::now() + Duration::from_secs(2);
    let token = biscuit!(
        r#"write("subscriptions"); read("channels"); check if time($time), $time < {expiry};"#,
        expiry = expiry
    )
    .build(&fixture.root)
    .unwrap()
    .to_base64()
    .unwrap();
    let client = fixture.client(Some(&token)).await;
    let mut subscription = subscribe(&client, SubscriptionTopic::ChannelSigning).await;
    let jsonrpsee::core::client::SubscriptionKind::Subscription(id) = subscription.kind() else {
        panic!("server subscription ID");
    };
    let id = id.clone();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let error = client
        .request::<bool, _>("unsubscribe", rpc_params![id])
        .await
        .unwrap_err();
    assert!(matches!(error, jsonrpsee::core::ClientError::Call(error) if error.code() == -32999));
    {
        let state = ChannelActorState::samples(42).remove(0);
        let publisher = fixture.events.channels(None);
        let update = publisher.prepare(&state);
        fixture.store.insert_channel_actor_state(state);
        update.publish();
    };
    let event = tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .unwrap();
    assert!(
        matches!(event, Some(Ok(SubscriptionEvent::ChannelSigning(_)))),
        "established subscriptions do not recheck credentials"
    );
    assert!(client
        .subscribe::<SubscriptionEvent, _>(
            "subscribe",
            rpc_params!["channel_signing"],
            "unsubscribe"
        )
        .await
        .is_err());
}

#[tokio::test]
async fn subscription_cannot_be_cancelled_from_another_connection_and_http_is_rejected() {
    let fixture = Fixture::new(false, true, false).await;
    let client = fixture.client(None).await;
    let other = fixture.client(None).await;
    let mut subscription = subscribe(&client, SubscriptionTopic::ChannelSigning).await;
    assert!(!other
        .request::<bool, _>(
            "unsubscribe",
            rpc_params![match subscription.kind() {
                jsonrpsee::core::client::SubscriptionKind::Subscription(id) => id.clone(),
                _ => unreachable!(),
            }]
        )
        .await
        .unwrap());
    {
        let state = ChannelActorState::samples(42).remove(0);
        let publisher = fixture.events.channels(None);
        let update = publisher.prepare(&state);
        fixture.store.insert_channel_actor_state(state);
        update.publish();
    };
    assert!(matches!(
        next(&mut subscription).await,
        SubscriptionEvent::ChannelSigning(_)
    ));
    let http = jsonrpsee::http_client::HttpClientBuilder::default()
        .build(format!("http://{}", fixture.address))
        .unwrap();
    assert!(http
        .request::<String, _>("subscribe", rpc_params!["channel_signing"])
        .await
        .is_err());
    subscription.unsubscribe().await.unwrap();
}

#[cfg(feature = "watchtower")]
#[tokio::test]
async fn subscription_watchtower_uses_the_authorized_node_even_for_shared_channel_ids() {
    use crate::watchtower::WatchtowerStore;
    use fiber_types::{NodeId, Privkey, SettlementData};

    let fixture = Fixture::new(true, true, true).await;
    let alice = NodeId::from_bytes(vec![2; 33]);
    let bob = NodeId::from_bytes(vec![3; 33]);
    let channel = Hash256::from([42; 32]);
    for node in [&alice, &bob] {
        fixture.store.insert_watch_channel(
            node.clone(),
            channel,
            None,
            None,
            Privkey::from(&[1; 32]).pubkey(),
            Privkey::from(&[2; 32]).pubkey(),
            Privkey::from(&[3; 32]).pubkey(),
            Privkey::from(&[4; 32]).pubkey(),
            SettlementData {
                local_amount: 100,
                remote_amount: 200,
                tlcs: vec![],
            },
        );
    }
    // The query RPC also permits read("channels") for watchtower signing status.
    let token = fixture.token(&format!(
        r#"write("subscriptions"); read("channels"); node("{alice}");"#
    ));
    let client = fixture.client(Some(&token)).await;
    let mut subscription = subscribe(&client, SubscriptionTopic::WatchtowerSigning).await;
    {
        let signing_scope = fixture.events.watchtower(&bob);
        let signing_update = signing_scope.prepare_removed(channel);
        if fixture.store.remove_watch_channel(bob, channel) {
            signing_update.publish();
        }
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(100), subscription.next())
            .await
            .is_err()
    );
    {
        let signing_scope = fixture.events.watchtower(&alice);
        let signing_update = signing_scope.prepare_removed(channel);
        if fixture.store.remove_watch_channel(alice, channel) {
            signing_update.publish();
        }
    };
    assert!(
        matches!(next(&mut subscription).await, SubscriptionEvent::Removed { channel_id } if channel_id == channel.into())
    );
    subscription.unsubscribe().await.unwrap();
}

#[tokio::test]
async fn subscription_connection_limit_is_released_by_unsubscribe() {
    let fixture = Fixture::new(false, true, false).await;
    let client = fixture.client(None).await;
    let mut subscriptions = Vec::new();
    for _ in 0..32 {
        subscriptions.push(subscribe(&client, SubscriptionTopic::ChannelSigning).await);
    }
    assert!(client
        .subscribe::<SubscriptionEvent, _>(
            "subscribe",
            rpc_params!["channel_signing"],
            "unsubscribe"
        )
        .await
        .is_err());
    subscriptions.pop().unwrap().unsubscribe().await.unwrap();
    let replacement = subscribe(&client, SubscriptionTopic::ChannelSigning).await;
    replacement.unsubscribe().await.unwrap();
    for subscription in subscriptions {
        subscription.unsubscribe().await.unwrap();
    }
}

#[tokio::test]
async fn subscription_slow_queue_does_not_block_other_subscribers_and_requires_resync() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path()).unwrap();
    let events = crate::signing::SigningEvents::default();
    let mut module = RpcModule::new(());
    module
        .merge(
            SubscriptionRpcServerImpl::new(events.clone(), store.clone(), None, true, false)
                .into_rpc(),
        )
        .unwrap();
    module.extensions_mut().insert(RpcAuthorization {
        auth: Arc::new(BiscuitAuth::without_pubkey()),
        token: None,
    });
    let request = r#"{"jsonrpc":"2.0","id":1,"method":"subscribe","params":["channel_signing"]}"#;
    let (_, mut slow) = module.raw_json_request(request, 1).await.unwrap();
    let (_, mut fast) = module.raw_json_request(request, 1).await.unwrap();
    // The first live update fills the slow subscriber's one-message queue.
    let state = ChannelActorState::samples(42).remove(0);
    let id = state.get_id();
    {
        let publisher = events.channels(None);
        let update = publisher.prepare(&state);
        store.insert_channel_actor_state(state);
        update.publish();
    };
    let event = tokio::time::timeout(Duration::from_secs(1), fast.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(event.get()).unwrap()["params"]["result"]["type"],
        "channel_signing"
    );
    {
        let publisher = events.channels(None);
        let update = publisher.prepare_removed(id);
        store.delete_channel_actor_state(&id);
        update.publish();
    };
    let removed = tokio::time::timeout(Duration::from_secs(1), fast.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(removed.get()).unwrap()["params"]["result"]
            ["type"],
        "removed"
    );
    tokio::time::sleep(Duration::from_millis(5_200)).await;
    slow.recv().await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(1), slow.recv())
        .await
        .unwrap()
        .unwrap();
    let closed: serde_json::Value = serde_json::from_str(closed.get()).unwrap();
    assert!(closed["params"]["error"]
        .as_str()
        .unwrap()
        .contains("reconnect to recover pending requests"));
}
