//! Preserve the SDK result consumer and roll back the exact failed reservation.
use blueprint_sdk::core::error::BoxError;
use blueprint_sdk::{JobResult, alloy::sol_types::SolValue, tangle::TangleConsumer};
use futures_util::Sink;
use std::{
    pin::Pin,
    task::{Context, Poll},
};

pub struct LeaseConsumer {
    inner: TangleConsumer,
    pending: Option<[u8; 32]>,
    in_flight: bool,
}
impl LeaseConsumer {
    pub fn new(inner: TangleConsumer) -> Self {
        Self {
            inner,
            pending: None,
            in_flight: false,
        }
    }
}
impl Sink<JobResult> for LeaseConsumer {
    type Error = BoxError;
    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        if self.in_flight {
            match self.as_mut().poll_flush(cx) {
                Poll::Ready(Ok(())) => {}
                other => return other,
            }
        }
        Pin::new(&mut self.inner).poll_ready(cx)
    }
    fn start_send(mut self: Pin<&mut Self>, result: JobResult) -> Result<(), BoxError> {
        if let Ok(body) = result.body() {
            if let Ok(output) = crate::GpuLeaseOutput::abi_decode(body) {
                if output.schemaVersion == crate::SCHEMA_VERSION
                    && crate::allocator()
                        .allocation(output.leaseId.0)
                        .is_some_and(|a| !a.confirmed && a.endpoint_v1 == output.endpoint)
                {
                    self.pending = Some(output.leaseId.0);
                }
            }
        }
        self.in_flight = true;
        if let Err(e) = Pin::new(&mut self.inner).start_send(result) {
            if let Some(id) = self.pending.take() {
                crate::allocator().release(id)?;
            }
            return Err(e);
        }
        Ok(())
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        match Pin::new(&mut self.inner).poll_flush(cx) {
            Poll::Ready(result) => {
                self.in_flight = false;
                if let Some(id) = self.pending.take() {
                    let mutation = if result.is_ok() {
                        crate::allocator().confirm(id).map(|_| ())
                    } else {
                        crate::allocator().release(id).map(|_| ())
                    };
                    if let Err(e) = mutation {
                        return Poll::Ready(Err(Box::new(e)));
                    }
                }
                Poll::Ready(result)
            }
            Poll::Pending => Poll::Pending,
        }
    }
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), BoxError>> {
        match self.as_mut().poll_flush(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut self.inner).poll_close(cx),
            other => other,
        }
    }
}
