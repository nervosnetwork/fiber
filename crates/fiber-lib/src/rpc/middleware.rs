use std::borrow::Cow;
use std::future::Future;
use std::sync::Arc;

use anyhow::{anyhow, Error, Result};
use biscuit_auth::error::{RunLimit, Token as BiscuitError};
use hyper::header::AUTHORIZATION;
use hyper::HeaderMap;
use jsonrpsee::core::middleware::{Batch, BatchEntry, BatchEntryErr, Notification};
use jsonrpsee::server::middleware::rpc::RpcServiceT;
use jsonrpsee::types::{ErrorObject, ErrorObjectOwned, Id, Request};
use jsonrpsee::{Extensions, MethodResponse};

use crate::rpc::biscuit::{extract_node_id, extract_tenant_id};
use crate::rpc::tenant::AuthenticatedTenant;
use fiber_json_types::RpcContext;
use fiber_types::NodeId;

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

    fn inject_authenticated_tenant(
        &self,
        extensions: &mut Extensions,
        token: &biscuit_auth::Biscuit,
    ) -> Result<()> {
        if let Some(tenant_id) = extract_tenant_id(token)? {
            extensions.insert(AuthenticatedTenant(tenant_id));
        }
        Ok(())
    }

    /// Authorize the request
    fn auth_call(&self, req: &mut Request<'_>) -> Result<()> {
        if self.enable_auth {
            let token = self.auth_token()?;
            let (token, rule) = self.auth.check_permission(&req.method, &token)?;
            self.inject_authenticated_tenant(req.extensions_mut(), &token)?;
            if rule.require_rpc_context {
                let node_id = extract_node_id(&token)?;

                // Inject RpcContext as first param (node_id as String)
                let ctx = RpcContext {
                    node_id: node_id.to_string(),
                };
                self.inject_rpc_context(req, ctx);
            }
            Ok(())
        } else {
            // local rpc, auth token is none
            match self.auth.get_rule(&req.method) {
                Ok(rule) => {
                    if rule.require_rpc_context {
                        let node_id = NodeId::local();

                        // Inject RpcContext as first param (node_id as String)
                        let ctx = RpcContext {
                            node_id: node_id.to_string(),
                        };
                        self.inject_rpc_context(req, ctx);
                    }
                }
                Err(err) => {
                    tracing::debug!("Failed get_rule #{err:?}");
                    // no auth rule, but allow local rpc to proceed.
                }
            }
            Ok(())
        }
    }

    /// Authorize the notification
    fn auth_notify(&self, notify: &mut Notification<'_>) -> Result<()> {
        if self.enable_auth {
            let token = self.auth_token()?;
            let (token, _) = self.auth.check_permission(notify.method_name(), &token)?;
            self.inject_authenticated_tenant(notify.extensions_mut(), &token)
        } else {
            Ok(())
        }
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
        let auth_error = this
            .auth_call(&mut req)
            .err()
            .map(|err| auth_reject_error(&req.method, &err));

        async move {
            if let Some(err) = auth_error {
                return MethodResponse::error(req.id, err);
            }
            this.inner.call(req).await
        }
    }

    fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        let entries: Vec<_> = batch
            .into_iter()
            .filter_map(|entry| match entry {
                Ok(BatchEntry::Call(mut req)) => match self.auth_call(&mut req) {
                    Ok(()) => Some(Ok(BatchEntry::Call(req))),
                    Err(err) => {
                        let rpc_error = auth_reject_error(&req.method, &err);
                        Some(Err(BatchEntryErr::new(req.id, rpc_error)))
                    }
                },
                Ok(BatchEntry::Notification(mut notif)) => {
                    // ignore permissionless notification
                    match self.auth_notify(&mut notif) {
                        Ok(()) => Some(Ok(BatchEntry::Notification(notif))),
                        Err(err) => {
                            log_auth_rejection(notif.method_name(), &err);
                            None
                        }
                    }
                }
                Err(err) => Some(Err(err)),
            })
            .collect();

        self.inner.batch(Batch::from(entries))
    }

    fn notification<'a>(
        &self,
        mut n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        let this = self.clone();
        let auth_error = this
            .auth_notify(&mut n)
            .err()
            .map(|err| auth_reject_error(n.method_name(), &err));

        async move {
            if let Some(err) = auth_error {
                return MethodResponse::error(Id::Null, err);
            }
            this.inner.notification(n).await
        }
    }
}

fn auth_reject_message(error: &Error) -> &'static str {
    match error.downcast_ref::<BiscuitError>() {
        Some(BiscuitError::RunLimit(RunLimit::Timeout)) => {
            "Unauthorized: Biscuit authorization timed out"
        }
        Some(BiscuitError::RunLimit(RunLimit::TooManyFacts)) => {
            "Unauthorized: Biscuit authorization generated too many facts"
        }
        Some(BiscuitError::RunLimit(RunLimit::TooManyIterations)) => {
            "Unauthorized: Biscuit authorization exceeded the iteration limit"
        }
        Some(BiscuitError::RunLimit(RunLimit::UnexpectedQueryResult(_, _))) => {
            "Unauthorized: Biscuit authorization returned unexpected query results"
        }
        _ => "Unauthorized",
    }
}

fn log_auth_rejection(method: &str, error: &Error) {
    match error.downcast_ref::<BiscuitError>() {
        Some(BiscuitError::RunLimit(limit)) => {
            tracing::warn!(
                rpc_method = method,
                run_limit = ?limit,
                "Biscuit authorization failed"
            );
        }
        _ => {
            tracing::debug!(
                rpc_method = method,
                error = %error,
                "Biscuit authorization failed"
            );
        }
    }
}

fn auth_reject_error(method: &str, error: &Error) -> ErrorObjectOwned {
    log_auth_rejection(method, error);
    ErrorObject::owned(-32999, auth_reject_message(error), None::<()>)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_reject_error_reports_run_limit_reason() {
        let cases = [
            (
                RunLimit::Timeout,
                "Unauthorized: Biscuit authorization timed out",
            ),
            (
                RunLimit::TooManyFacts,
                "Unauthorized: Biscuit authorization generated too many facts",
            ),
            (
                RunLimit::TooManyIterations,
                "Unauthorized: Biscuit authorization exceeded the iteration limit",
            ),
            (
                RunLimit::UnexpectedQueryResult(1, 2),
                "Unauthorized: Biscuit authorization returned unexpected query results",
            ),
        ];

        for (limit, message) in cases {
            let error = Error::new(BiscuitError::RunLimit(limit));
            let rpc_error = auth_reject_error("test_method", &error);

            assert_eq!(rpc_error.code(), -32999);
            assert_eq!(rpc_error.message(), message);
        }
    }

    #[test]
    fn test_auth_reject_error_keeps_generic_unauthorized_message() {
        let error = anyhow!("missing token");
        let rpc_error = auth_reject_error("test_method", &error);

        assert_eq!(rpc_error.code(), -32999);
        assert_eq!(rpc_error.message(), "Unauthorized");
    }
}
