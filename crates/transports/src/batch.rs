use std::{
    collections::HashMap,
    future::{Future, IntoFuture},
    marker::PhantomData,
    pin::Pin,
    task::{self, ready},
};

use futures_channel::oneshot;
use serde_json::value::RawValue;
use tower::Service;

use crate::{
    error::TransportError,
    transports::{BatchFutureOf, Transport},
    RpcClient,
};
use alloy_json_rpc::{Id, JsonRpcRequest, RpcParam, RpcResult, RpcReturn};

type Channel = oneshot::Sender<RpcResult<Box<RawValue>, TransportError>>;
type ChannelMap = HashMap<Id, Channel>;

#[must_use = "A BatchRequest does nothing unless sent via `send_batch` and `.await`"]
/// A Batch JSON-RPC request, awaiting dispatch.
#[derive(Debug)]
pub struct BatchRequest<'a, T> {
    transport: &'a RpcClient<T>,

    requests: Vec<JsonRpcRequest>,

    channels: ChannelMap,
}

/// Awaits a single response for a request that has been included in a batch.
pub struct Waiter<Resp> {
    rx: oneshot::Receiver<RpcResult<Box<RawValue>, TransportError>>,
    _resp: PhantomData<Resp>,
}

impl<Resp> From<oneshot::Receiver<RpcResult<Box<RawValue>, TransportError>>> for Waiter<Resp> {
    fn from(rx: oneshot::Receiver<RpcResult<Box<RawValue>, TransportError>>) -> Self {
        Self {
            rx,
            _resp: PhantomData,
        }
    }
}

impl<Resp> std::future::Future for Waiter<Resp>
where
    Resp: RpcReturn,
{
    type Output = RpcResult<Resp, TransportError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Self::Output> {
        let resp = ready!(Pin::new(&mut self.rx).poll(cx));

        task::Poll::Ready(match resp {
            Ok(resp) => resp.deser_ok_or_else(|e, text| TransportError::deser_err(e, text)),
            Err(e) => RpcResult::Err(TransportError::Custom(Box::new(e))),
        })
    }
}

#[pin_project::pin_project(project = CallStateProj)]
pub enum BatchFuture<Conn>
where
    Conn: Transport,
{
    Prepared {
        transport: Conn,
        requests: Vec<JsonRpcRequest>,
        channels: ChannelMap,
    },
    SerError(Option<TransportError>),
    AwaitingResponse {
        channels: ChannelMap,
        #[pin]
        fut: BatchFutureOf<Conn>,
    },
    Complete,
}

impl<'a, T> BatchRequest<'a, T> {
    pub fn new(transport: &'a RpcClient<T>) -> Self {
        Self {
            transport,
            requests: Vec::with_capacity(10),
            channels: HashMap::with_capacity(10),
        }
    }

    fn push_raw(
        &mut self,
        request: JsonRpcRequest,
    ) -> oneshot::Receiver<RpcResult<Box<RawValue>, TransportError>> {
        let (tx, rx) = oneshot::channel();
        self.channels.insert(request.id.clone(), tx);
        self.requests.push(request);
        rx
    }

    fn push<Resp: RpcReturn>(&mut self, request: JsonRpcRequest) -> Waiter<Resp> {
        self.push_raw(request).into()
    }
}

impl<'a, T> BatchRequest<'a, T>
where
    T: Transport,
{
    #[must_use = "Waiters do nothing unless polled. A Waiter will never resolve unless its batch is sent."]
    /// Add a call to the batch.
    pub fn add_call<Params: RpcParam, Resp: RpcReturn>(
        &mut self,
        method: &'static str,
        params: Params,
    ) -> Waiter<Resp> {
        let request = self.transport.make_request(method, params).unwrap();
        self.push(request)
    }

    /// Send the batch future via its connection.
    pub fn send_batch(self) -> BatchFuture<T> {
        BatchFuture::Prepared {
            transport: self.transport.transport.clone(),
            requests: self.requests,
            channels: self.channels,
        }
    }
}

impl<'a, T> IntoFuture for BatchRequest<'a, T>
where
    T: Transport,
{
    type Output = <BatchFuture<T> as Future>::Output;
    type IntoFuture = BatchFuture<T>;

    fn into_future(self) -> Self::IntoFuture {
        self.send_batch()
    }
}

impl<T> BatchFuture<T>
where
    T: Transport,
{
    fn poll_prepared(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<<Self as Future>::Output> {
        let CallStateProj::Prepared {
            transport,
            requests,
            channels,
        } = self.as_mut().project()
        else {
            unreachable!("Called poll_prepared in incorrect state")
        };

        if let Err(e) = task::ready!(<T as Service<Vec<JsonRpcRequest>>>::poll_ready(
            transport, cx
        )) {
            self.set(BatchFuture::Complete);
            return task::Poll::Ready(Err(e));
        }

        // We only have mut refs, and we want ownership, so we just replace
        // with 0-capacity collections.
        let channels = std::mem::replace(channels, HashMap::with_capacity(0));
        let requests = std::mem::replace(requests, Vec::with_capacity(0));

        let fut = transport.call(requests);

        self.set(BatchFuture::AwaitingResponse { channels, fut });
        cx.waker().wake_by_ref();

        task::Poll::Pending
    }

    fn poll_awaiting_response(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<<Self as Future>::Output> {
        let CallStateProj::AwaitingResponse { channels, fut } = self.as_mut().project() else {
            unreachable!("Called poll_awaiting_response in incorrect state")
        };

        // Has the service responded yet?
        let responses = match ready!(fut.poll(cx)) {
            Ok(responses) => responses,
            Err(e) => {
                self.set(BatchFuture::Complete);
                return task::Poll::Ready(Err(e));
            }
        };

        // Drain the responses into the channels.
        for response in responses {
            if let Some(tx) = channels.remove(&response.id) {
                let _ = tx.send(RpcResult::from(response));
            }
        }

        // Any remaining channels are missing responses.
        channels.drain().for_each(|(_, tx)| {
            let _ = tx.send(RpcResult::Err(TransportError::MissingBatchResponse));
        });

        self.set(BatchFuture::Complete);
        task::Poll::Ready(Ok(()))
    }

    fn poll_ser_error(
        mut self: Pin<&mut Self>,
        _cx: &mut task::Context<'_>,
    ) -> task::Poll<<Self as Future>::Output> {
        let e = if let CallStateProj::SerError(e) = self.as_mut().project() {
            e.take().expect("No error. This is a bug.")
        } else {
            unreachable!("Called poll_ser_error in incorrect state")
        };

        self.set(BatchFuture::Complete);
        task::Poll::Ready(Err(e))
    }
}

impl<T> Future for BatchFuture<T>
where
    T: Transport,
{
    type Output = Result<(), TransportError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> task::Poll<Self::Output> {
        if matches!(*self.as_mut(), BatchFuture::Prepared { .. }) {
            return self.poll_prepared(cx);
        }

        if matches!(*self.as_mut(), BatchFuture::AwaitingResponse { .. }) {
            return self.poll_awaiting_response(cx);
        }

        if matches!(*self.as_mut(), BatchFuture::SerError(_)) {
            return self.poll_ser_error(cx);
        }

        panic!("Called poll on CallState in invalid state")
    }
}
