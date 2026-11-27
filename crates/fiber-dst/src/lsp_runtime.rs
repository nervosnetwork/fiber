//! Hosted LSP service and production Biscuit-authenticated JSON-RPC stack in-memory for Fiber DST.

use std::path::PathBuf;
use std::sync::Arc;

use biscuit_auth::{macros::biscuit, Algorithm, KeyPair, PrivateKey};
use ckb_types::packed::Script;
use fnn::fiber::network::{NetworkActorMessage, PublicNetworkCommand};
use fnn::fiber::{FiberConfig, Privkey};
use fnn::lsp::{
    BiscuitTokenIssuer, FiberTenantRuntimeFactory, LspConfig, LspDeliveryPort, LspService,
    LspServiceArgs, LspServiceMessage,
};
use fnn::rpc::{
    biscuit::BiscuitAuth,
    channel::{ChannelRpcServer, ChannelRpcServerImpl},
    invoice::{InvoiceRpcServer, InvoiceRpcServerImpl},
    lsp::{LspRpcServer, LspRpcServerImpl},
    middleware::BiscuitAuthMiddleware,
    payment::{PaymentRpcServer, PaymentRpcServerImpl},
    watchtower::{WatchtowerRpcServer, WatchtowerRpcServerImpl},
};
use fnn::simulation::SimulatedNode;
use fnn::store::{NodeNamespace, Store};
use hyper::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use jsonrpsee::server::HttpBody;
use jsonrpsee::ws_client::RpcServiceBuilder;
use jsonrpsee::RpcModule;
use ractor::{Actor, ActorRef};
use tower::Service;

/// Hosted LSP service and production Biscuit-authenticated JSON-RPC stack running in-memory.
pub struct SimulatedLspRuntime {
    pub lsp_actor: ActorRef<LspServiceMessage>,
    pub admin_token: String,
    methods: jsonrpsee::Methods,
    auth: Arc<BiscuitAuth>,
    _stop_handle: jsonrpsee::server::ServerHandle,
    stop_handle: jsonrpsee::server::StopHandle,
}

impl Drop for SimulatedLspRuntime {
    fn drop(&mut self) {
        fn unlink_children(parent: ractor::ActorCell) {
            for child in parent.get_children() {
                child.unlink(parent.clone());
                unlink_children(child);
            }
        }
        unlink_children(self.lsp_actor.get_cell());
    }
}

impl SimulatedLspRuntime {
    /// Execute a single raw JSON-RPC request through production `BiscuitAuthMiddleware`.
    pub async fn call_rpc(
        &self,
        bearer_token: Option<&str>,
        raw_request: &str,
    ) -> anyhow::Result<String> {
        let mut req_builder = hyper::Request::builder()
            .method(hyper::Method::POST)
            .uri("/")
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(token) = bearer_token {
            req_builder = req_builder.header(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))?,
            );
        }
        let req = req_builder.body(HttpBody::from(raw_request.to_owned()))?;
        let headers = req.headers().clone();
        let auth = self.auth.clone();
        let rpc_middleware =
            RpcServiceBuilder::new().layer_fn(move |service| BiscuitAuthMiddleware {
                headers: headers.clone(),
                inner: service,
                auth: auth.clone(),
                enable_auth: true,
            });
        let mut svc = jsonrpsee::server::Server::builder()
            .to_service_builder()
            .set_rpc_middleware(rpc_middleware)
            .build(self.methods.clone(), self.stop_handle.clone());
        let response = svc
            .call(req)
            .await
            .map_err(|err| anyhow::anyhow!("in-memory LSP RPC service error: {err}"))?;
        let mut body = response.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = std::future::poll_fn(|cx| {
            hyper::body::Body::poll_frame(std::pin::Pin::new(&mut body), cx)
        })
        .await
        {
            let frame = frame.map_err(|err| anyhow::anyhow!("read LSP RPC body: {err}"))?;
            if let Ok(data) = frame.into_data() {
                bytes.extend_from_slice(&data);
            }
        }
        Ok(String::from_utf8(bytes)?)
    }
}

/// Start a hosted `LspService`, attach its delivery port to `node`, and assemble the
/// production Biscuit-authenticated JSON-RPC handlers for in-memory simulation dispatch.
#[allow(clippy::too_many_arguments)]
pub async fn start_lsp_runtime(
    node: &SimulatedNode,
    mut template_config: FiberConfig,
    private_key: Privkey,
    store: Store,
    lsp_dir: PathBuf,
    biscuit_seed: [u8; 32],
    mut lsp_config: LspConfig,
) -> anyhow::Result<SimulatedLspRuntime> {
    std::fs::create_dir_all(&lsp_dir)?;
    let public_base_dir = lsp_dir.join("public");
    std::fs::create_dir_all(&public_base_dir)?;
    let sk_path = public_base_dir.join("sk");
    if !sk_path.exists() {
        let keypair = fnn::fiber::KeyPair::try_from(private_key.as_ref().as_slice())
            .map_err(|err| anyhow::anyhow!("invalid LSP public keypair: {err}"))?;
        keypair.write_to_file(&sk_path)?;
    }
    template_config.set_base_dir(public_base_dir);
    lsp_config.set_base_dir(lsp_dir.clone());

    let biscuit_priv = PrivateKey::from_bytes(&biscuit_seed, Algorithm::Ed25519)
        .map_err(|err| anyhow::anyhow!("deterministic Biscuit key: {err}"))?;
    let biscuit_root = KeyPair::from(&biscuit_priv);
    let biscuit_public_key = biscuit_root.public().to_string();
    let token_issuer = BiscuitTokenIssuer::from_private_key(
        &biscuit_root.private().to_prefixed_string(),
        &biscuit_public_key,
    )?;

    let runtime_factory = Arc::new(FiberTenantRuntimeFactory::new(
        node.signing_events.clone(),
        lsp_config.clone(),
        template_config.clone(),
        node.chain_client.clone(),
        node.chain_actor.clone(),
        node.actor.clone(),
        store.clone(),
        node.actor.get_cell(),
        Script::default(),
    ));
    let (lsp_actor, _) = Actor::spawn_linked(
        None,
        LspService,
        LspServiceArgs {
            config: lsp_config.clone(),
            public_node_id: private_key.pubkey(),
            public_network_actor: node.actor.clone(),
            store: store.namespaced(NodeNamespace::lsp_metadata()),
            runtime_factory,
            signing_key: private_key,
            token_issuer,
            watchtower_store: store.clone(),
            signing_events: node.signing_events.clone(),
        },
        node.actor.get_cell(),
    )
    .await?;

    node.actor
        .send_message(NetworkActorMessage::new_command(
            PublicNetworkCommand::SetDeliveryPort(LspDeliveryPort::new(
                lsp_actor.clone(),
                store.clone(),
            )),
        ))
        .map_err(|err| anyhow::anyhow!("attach LSP delivery port: {err}"))?;

    let token_epochs = ractor::call!(lsp_actor, LspServiceMessage::GetTenantTokenEpochs)
        .map_err(|err| anyhow::anyhow!("query LSP token epochs: {err}"))?;
    let watchtower_quotas = ractor::call!(lsp_actor, LspServiceMessage::GetWatchtowerQuotas)
        .map_err(|err| anyhow::anyhow!("query LSP watchtower quotas: {err}"))?;

    let auth = Arc::new(
        BiscuitAuth::from_pubkey(biscuit_public_key.clone())?
            .with_lsp_enabled(true)
            .with_tenant_token_epochs(token_epochs),
    );
    let admin_token = biscuit!(
        r#"
            read("lsp");
            write("lsp");
            read("channels");
            write("channels");
            read("invoices");
            write("invoices");
            read("payments");
            write("payments");
            read("watchtower");
            write("watchtower");
        "#
    )
    .build(&biscuit_root)
    .map_err(|err| anyhow::anyhow!("build admin biscuit: {err}"))?
    .to_base64()
    .map_err(|err| anyhow::anyhow!("encode admin biscuit: {err}"))?;

    let mut modules = RpcModule::new(());
    modules.merge(
        InvoiceRpcServerImpl::new(
            store.clone(),
            Some(node.actor.clone()),
            Some(template_config),
        )
        .with_lsp_actor(Some(lsp_actor.clone()))
        .into_rpc(),
    )?;
    modules.merge(
        ChannelRpcServerImpl::new(node.actor.clone(), store.clone())
            .with_lsp_actor(Some(lsp_actor.clone()))
            .into_rpc(),
    )?;
    modules.merge(
        PaymentRpcServerImpl::new(node.actor.clone(), store.clone())
            .with_lsp_actor(Some(lsp_actor.clone()))
            .into_rpc(),
    )?;
    modules.merge(
        WatchtowerRpcServerImpl::new(store)
            .with_signing_events(node.signing_events.clone())
            .with_quotas(watchtower_quotas)
            .with_lsp_enabled(true)
            .with_lsp_actor(Some(lsp_actor.clone()))
            .into_rpc(),
    )?;
    modules.merge(LspRpcServerImpl::new(lsp_actor.clone()).into_rpc())?;

    let (stop_handle, server_handle) = jsonrpsee::server::stop_channel();
    Ok(SimulatedLspRuntime {
        lsp_actor,
        admin_token,
        methods: modules.into(),
        auth,
        _stop_handle: server_handle,
        stop_handle,
    })
}
