use std::sync::Arc;

use hyper::header::AUTHORIZATION;
use hyper::HeaderMap;
use jsonrpsee::core::middleware::{Batch, BatchEntry, BatchEntryErr, Notification};
use jsonrpsee::server::middleware::rpc::RpcServiceT;
use jsonrpsee::types::{ErrorObject, ErrorObjectOwned, Id, Request};
use jsonrpsee::MethodResponse;
use std::future::Future;

use super::biscuit::BiscuitAuth;

const BEARER_PREFIX: &str = "Bearer ";

#[derive(Clone)]
pub struct BiscuitAuthMiddleware<S> {
    pub headers: HeaderMap,
    pub inner: S,
    pub auth: Arc<BiscuitAuth>,
}

impl<S> BiscuitAuthMiddleware<S> {
    fn auth_token(&self) -> Option<&str> {
        self.headers
            .get(AUTHORIZATION)
            .and_then(|auth| auth.to_str().ok())
            .and_then(|auth_str| auth_str.strip_prefix(BEARER_PREFIX))
    }

    /// Authorize the request
    fn auth_call(&self, req: &Request<'_>) -> bool {
        let Some(auth_token) = self.auth_token() else {
            return false;
        };

        let res = self
            .auth
            .check_permission(&req.method, auth_token.as_bytes());
        res.is_ok()
    }

    /// Authorize the notification
    fn auth_notify(&self, notify: &Notification<'_>) -> bool {
        let Some(auth_token) = self.auth_token() else {
            return false;
        };
        let res = self
            .auth
            .check_permission(notify.method_name(), auth_token.as_bytes());
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

    fn call<'a>(&self, req: Request<'a>) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        let this = self.clone();
        let auth_ok = this.auth_call(&req);

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
                Ok(BatchEntry::Call(req)) => {
                    if self.auth_call(&req) {
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

#[derive(Clone)]
pub struct IdentityLayer;

impl<S> tower::Layer<S> for IdentityLayer
where
    S: RpcServiceT + Send + Sync + Clone + 'static,
{
    type Service = Identity<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Identity(inner)
    }
}

#[derive(Clone)]
pub struct Identity<S>(pub S);

impl<S> RpcServiceT for Identity<S>
where
    S: RpcServiceT + Send + Sync + Clone + 'static,
{
    type MethodResponse = S::MethodResponse;
    type BatchResponse = S::BatchResponse;
    type NotificationResponse = S::NotificationResponse;

    fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        self.0.batch(batch)
    }

    fn call<'a>(
        &self,
        request: Request<'a>,
    ) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        self.0.call(request)
    }

    fn notification<'a>(
        &self,
        n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        self.0.notification(n)
    }
}

fn auth_reject_error() -> ErrorObjectOwned {
    ErrorObject::owned(-32999, "Unauthorized", None::<()>)
}
