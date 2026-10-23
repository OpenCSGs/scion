//! Sends git's smart HTTP requests to the git gateway before the gRPC layers see them, so one
//! listener and one repository URL serve both `scn` and git.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use scion_git::Gateway;
use tower::{Layer, Service};

#[derive(Clone)]
pub struct GitLayer {
    gateway: Option<Arc<Gateway>>,
}

impl GitLayer {
    /// `None` leaves every request to the inner service.
    pub fn new(gateway: Option<Arc<Gateway>>) -> Self {
        Self { gateway }
    }
}

impl<S> Layer<S> for GitLayer {
    type Service = GitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        GitService {
            inner,
            gateway: self.gateway.clone(),
        }
    }
}

#[derive(Clone)]
pub struct GitService<S> {
    inner: S,
    gateway: Option<Arc<Gateway>>,
}

type BoxFuture<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send>>;

impl<S, ReqBody, ResBody> Service<http::Request<ReqBody>> for GitService<S>
where
    S: Service<http::Request<ReqBody>, Response = http::Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ReqBody: http_body::Body<Data = Bytes> + Send + 'static,
    ReqBody::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    ResBody: http_body::Body<Data = Bytes> + Send + 'static,
    ResBody::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = S::Error;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqBody>) -> Self::Future {
        let git = self
            .gateway
            .as_ref()
            .and_then(|g| scion_git::route(req.method(), req.uri().path(), req.headers()).map(|r| (g.clone(), r)));
        if let Some((gateway, (repo, endpoint))) = git {
            return Box::pin(async move { Ok(gateway.serve(req, repo, endpoint).await.map(tonic::body::Body::new)) });
        }
        let fut = self.inner.call(req);
        Box::pin(async move { Ok(fut.await?.map(tonic::body::Body::new)) })
    }
}
