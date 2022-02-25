#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]

use std::sync::{Arc,Weak,Mutex,RwLock};
use tokio::sync;
use tokio::time;
use std::fmt;
use core::task;
use core::future::Future;
use core::pin::Pin;

#[derive(Debug,Clone,PartialEq)]
pub enum Err {
    DeadlineExceeded,
    ContextCancelled,
}

pub trait CastError: fmt::Display+fmt::Debug+Send+Sync+std::cmp::PartialEq+'static {}
impl<T> CastError for T where T: fmt::Display+fmt::Debug+Send+Sync+std::cmp::PartialEq+'static {}

pub trait AnyhowCast {
    fn matches<E:CastError> (&self,err:&E) -> bool;
}

impl AnyhowCast for anyhow::Error {
    fn matches<E:CastError>(&self,err:&E) -> bool {
        return self.downcast_ref::<E>().map(|e|e==err).unwrap_or(false);
    }
}

impl<T> AnyhowCast for anyhow::Result<T> {
    fn matches<E:CastError>(&self,err:&E) -> bool {
        return self.as_ref().err().map(|e|e.matches(err)).unwrap_or(false);
    }
}

impl fmt::Display for Err {
    fn fmt(&self, f : &mut fmt::Formatter) -> fmt::Result {
        return write!(f,"{:?}",self);
    }
}

impl std::error::Error for Err {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { return None }
}

struct Eventual_<T:Clone> {
    value:Option<T>,
    waiters: Vec<Weak<RwLock<Waiter_<T>>>>,
}

struct Waiter_<T:Clone> {
    waker:Option<task::Waker>,
    e:Arc<RwLock<Eventual_<T>>>,
}

pub struct Eventual<T:Clone>(Arc<RwLock<Eventual_<T>>>);
pub struct Waiter<T:Clone>(Arc<RwLock<Waiter_<T>>>);

impl<T:Clone> Eventual<T> {
    fn new() -> Eventual<T> {
        return Eventual(Arc::new(RwLock::new(Eventual_{
            value: None,
            waiters: vec![],
        }))); 
    }

    fn set(&self, v : T) -> bool {
        let ws = {
            let mut e = self.0.write().unwrap();
            if e.value.is_some() { return false; }
            e.value = Some(v);
            e.waiters.split_off(0)
        };
        for w in ws.iter().map(|w|w.upgrade()).flatten() {
            let w = w.read().unwrap();
            if let Some(w) = &w.waker { w.wake_by_ref(); }
        }
        return true;
    }
   
    fn get(&self) -> Option<T> {
        return self.0.read().unwrap().value.clone();
    }

    fn wait(&self) -> Waiter<T> {
        let mut e = self.0.write().unwrap();
        let w = Arc::new(RwLock::new(Waiter_{waker:None,e:self.0.clone()}));
        e.waiters.push(Arc::downgrade(&w));
        return Waiter(w);
    }
}

impl<T:Clone> Drop for Waiter_<T> {
    fn drop(&mut self) {
        let mut e = self.e.write().unwrap();
        e.waiters.retain(|w|w.upgrade().is_some());
    }
}

impl<T:Clone> Future for Waiter<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> task::Poll<T> {
        let mut w = self.0.write().unwrap();
        let e = w.e.clone();
        let e = e.read().unwrap();
        match &e.value {
            Some(v) => task::Poll::Ready(v.clone()),
            None => { w.waker = Some(cx.waker().clone()); task::Poll::Pending }
        }
    }
}

struct Ctx_ {
    done : Eventual<Err>,
    deadline : Option<time::Instant>,
    parent : Option<Arc<Ctx_>>,
    children : RwLock<Vec<Weak<Ctx_>>>,
}

#[derive(Clone)]
pub struct Ctx(Arc<Ctx_>);

impl Drop for Ctx_ {
    fn drop(&mut self) {
        self.parent.clone().map(
            |p|p.children
                .write().unwrap()
                .retain(|c|c.upgrade().is_some())
        );
    }
}

impl Ctx {
    pub fn background() -> Ctx {
        return Ctx(Arc::new(Ctx_{
            parent: None,
            deadline: None,
            done: Eventual::new(),
            children: RwLock::new(vec![]),
        }));
    }

    fn cancel(&self) {
        if !self.0.done.set(Err::ContextCancelled) { return; }
        let _ = self.0.children.write().unwrap().drain(0..)
            .map(|c|c.upgrade()).flatten()
            .map(|c|Ctx(c).cancel());
    }

    pub fn err(&self) -> Option<Err> { self.0.done.get() }

    pub async fn done(&self) -> Err {
        let x = self.clone();
        match x.0.deadline {
            Some(d) => match time::timeout_at(d,x.0.done.wait()).await {
                Err(_) => Err::DeadlineExceeded,
                Ok(e) => e,
            }
            None => x.0.done.wait().await,
        }
    }

    pub async fn wrap<F,T>(&self,f:F) -> Result<T,Err> where
        F : std::future::Future<Output=T>,
    {
        tokio::select!{
            v = f => return Ok(v),
            v = self.done() => return Err(v),
        }
    }

    pub fn with_cancel(&self) -> (Ctx,impl Fn()->()) {
        let mut children = self.0.children.write().unwrap();
        let done = if let Some(_) = self.0.done.get() {
            Eventual(self.0.done.0.clone())
        } else {
            Eventual::new()
        };
        let ctx = Ctx(Arc::new(Ctx_{
            parent: Some(self.0.clone()),
            done: done, 
            deadline: self.0.deadline,
            children: RwLock::new(vec![]),
        }));
        children.push(Arc::downgrade(&ctx.0));
        let ctx1 = ctx.clone();
        return (ctx,move ||ctx1.cancel());
    }

    pub fn with_deadline(&self, deadline: tokio::time::Instant) -> Ctx {
        let mut children = self.0.children.write().unwrap();
        let ctx = Ctx(Arc::new(Ctx_{
            parent: Some(self.0.clone()),
            done: Eventual(self.0.done.0.clone()),
            deadline: Some(std::cmp::min(deadline,self.0.deadline.unwrap_or(deadline))),
            children: RwLock::new(vec![]),
        }));
        children.push(Arc::downgrade(&ctx.0));
        return ctx;
    }

    pub fn with_timeout(&self, timeout : time::Duration) -> Ctx {
        return self.with_deadline(time::Instant::now()+timeout)
    }
}
