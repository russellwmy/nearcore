use crate::concurrency::{Ctx,CtxErr};

use std::sync::{Arc};
use std::future::Future;
use std::pin::Pin;
use futures::future::FutureExt;
use futures::task;
struct DummyWake;

impl task::ArcWake for DummyWake {
    fn wake_by_ref(arc_self:&Arc<Self>){}
}

fn try_await<F,T>(f:Pin<&mut F>) -> Option<T> where
    F : Future<Output=T>,
{
    match f.poll(&mut task::Context::from_waker(
        &task::waker(Arc::new(DummyWake{}))
    )) {
        task::Poll::Ready(v) => Some(v),
        task::Poll::Pending => None,
    }
}

#[tokio::test]
async fn test_cancel_propagation() {
    let (ctx1,cancel1) = Ctx::background().with_cancel();
    let (ctx2,_) = ctx1.with_cancel();
    let (ctx3,cancel3) = ctx1.with_cancel();
    let mut h1 = Box::pin(ctx1.done());
    let mut h2 = Box::pin(ctx2.done());
    let mut h3 = Box::pin(ctx3.done());
    assert!(ctx1.err().is_none());
    assert!(ctx2.err().is_none());
    assert!(ctx3.err().is_none());

    cancel3();
    assert!(ctx1.err().is_none());
    assert!(ctx2.err().is_none());
    assert!(ctx3.err().is_some());
    assert_eq!(None,try_await(h1.as_mut()));
    assert_eq!(None,try_await(h2.as_mut()));
    assert_eq!(Some(CtxErr::Cancelled),try_await(h3.as_mut()));

    cancel1();
    assert!(ctx1.err().is_some());
    assert!(ctx2.err().is_some());
    assert!(ctx3.err().is_some());
    assert_eq!(Some(CtxErr::Cancelled),try_await(h1.as_mut()));
    assert_eq!(Some(CtxErr::Cancelled),try_await(h2.as_mut()));
}
