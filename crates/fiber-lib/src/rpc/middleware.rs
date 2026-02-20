use std::borrow::Cow;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use hyper::header::AUTHORIZATION;
use hyper::HeaderMap;
use jsonrpsee::core::middleware::{Batch, BatchEntry, BatchEntryErr, Notification};
use jsonrpsee::server::middleware::rpc::RpcServiceT;
use jsonrpsee::types::{ErrorObject, ErrorObjectOwned, Id, Request};
use jsonrpsee::MethodResponse;
use std::future::Future;

use crate::fiber::types::NodeId;
use crate::rpc::biscuit::extract_node_id;
use crate::rpc::context::RpcContext;

use super::biscuit::BiscuitAuth;

const BEARER_PREFIX: &str = "Bearer ";

#[derive(Clone)]
pub struct BiscuitAuthMiddleware<S> {
    pub headers: HeaderMap,
    pub inner: S,
    pub auth: Arc<BiscuitAuth>,
    pub enable_auth: bool,
}

impl<S> BiscuitAuthMiddleware<S> {
    fn auth_token(&self) -> Result<String> {
        let auth_str = self
            .headers
            .get(AUTHORIZATION)
            .ok_or_else(|| anyhow!("no authorization header"))?
            .to_str()?;
        let token = auth_str
            .strip_prefix(BEARER_PREFIX)
            .ok_or_else(|| anyhow!("invalid authorization header"))?;
        Ok(token.to_string())
    }

    fn extract_params(&self, params: serde_json::Value) -> Option<serde_json::Value> {
        params.as_array()?.first().cloned()
    }

    fn inject_rpc_context(&self, req: &mut Request<'_>, ctx: RpcContext) {
        let body = req
            .params()
            .parse::<serde_json::Value>()
            .unwrap_or_default();

        let params = self.extract_params(body).unwrap_or_default();
        req.params = Some(Cow::Owned(
            serde_json::value::to_raw_value(&[serde_json::json!(ctx), params])
                .expect("serialize injected params"),
        ));
        tracing::trace!("Injected req params {:?}", &req.params);
    }

    /// Authorize the request
    fn auth_call(&self, req: &mut Request<'_>) -> bool {
        if self.enable_auth {
            // extract auth token
            let token = match self.auth_token() {
                Ok(token) => token,
                Err(err) => {
                    tracing::debug!("failed to get auth token: {err}");
                    return false;
                }
            };

            match self.auth.check_permission(&req.method, &token) {
                Ok((token, rule)) => {
                    if rule.require_rpc_context {
                        let Ok(node_id) = extract_node_id(&token) else {
                            return false;
                        };

                        // Inject RpcContext as first param
                        let ctx = RpcContext { node_id };
                        self.inject_rpc_context(req, ctx);
                    }
                    return true;
                }
                Err(err) => {
                    tracing::debug!("Failed check_permission #{err:?}");
                    return false;
                }
            }
        } else {
            // local rpc, auth token is none
            match self.auth.get_rule(&req.method) {
                Ok(rule) => {
                    if rule.require_rpc_context {
                        let node_id = NodeId::local();

                        // Inject RpcContext as first param
                        let ctx = RpcContext { node_id };
                        self.inject_rpc_context(req, ctx);
                    }
                    return true;
                }
                Err(err) => {
                    tracing::debug!("Failed check_permission #{err:?}");
                    return false;
                }
            }
        }
    }

    /// Authorize the notification
    fn auth_notify(&self, notify: &Notification<'_>) -> bool {
        let token = match self.auth_token() {
            Ok(token) => token,
            Err(err) => {
                tracing::debug!("failed to get auth token: {err}");
                return false;
            }
        };
        let res = self.auth.check_permission(notify.method_name(), &token);
        res.is_ok()
    }
}

impl<S> RpcServiceT for BiscuitAuthMiddleware<S>
where
    S: RpcServiceT<
            MethodResponse = MethodResponse,
            BatchResponse = MethodResponse,
            NotificationResponse = MethodResponse,
        > + Send
        + Sync
        + Clone
        + 'static,
{
    type MethodResponse = S::MethodResponse;
    type BatchResponse = S::BatchResponse;
    type NotificationResponse = S::NotificationResponse;

    fn call<'a>(
        &self,
        mut req: Request<'a>,
    ) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        let this = self.clone();
        let auth_ok = this.auth_call(&mut req);

        async move {
            if !auth_ok {
                return MethodResponse::error(req.id, auth_reject_error());
            }
            this.inner.call(req).await
        }
    }

    fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        let entries: Vec<_> = batch
            .into_iter()
            .filter_map(|entry| match entry {
                Ok(BatchEntry::Call(mut req)) => {
                    if self.auth_call(&mut req) {
                        Some(Ok(BatchEntry::Call(req)))
                    } else {
                        Some(Err(BatchEntryErr::new(req.id, auth_reject_error())))
                    }
                }
                Ok(BatchEntry::Notification(notif)) => {
                    // ignore permissionless notification
                    if self.auth_notify(&notif) {
                        Some(Ok(BatchEntry::Notification(notif)))
                    } else {
                        None
                    }
                }
                Err(err) => Some(Err(err)),
            })
            .collect();

        self.inner.batch(Batch::from(entries))
    }

    fn notification<'a>(
        &self,
        n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        let this = self.clone();
        let auth_ok = this.auth_notify(&n);

        async move {
            if !auth_ok {
                return MethodResponse::error(Id::Null, auth_reject_error());
            }
            this.inner.notification(n).await
        }
    }
}

fn auth_reject_error() -> ErrorObjectOwned {
    ErrorObject::owned(-32999, "Unauthorized", None::<()>)
}
