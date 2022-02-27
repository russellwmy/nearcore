#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]

use std::sync::{Arc,Weak,Mutex,RwLock};
use std::marker::PhantomData;
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
                Err(_) => {
                    x.0.done.set(Err::DeadlineExceeded);
                    x.0.done.get() // get(), because there can be a race condition on set().
                }
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

    pub async fn wait(&self, duration:tokio::Duration) -> anyhow::Result<()> {
        self.wrap(tokio::delay_for(duration))?;
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
