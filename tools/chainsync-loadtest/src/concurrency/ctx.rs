#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]

use crate::concurrency::Eventual;
use std::sync::{Arc,Weak,Mutex,RwLock};
use std::marker::PhantomData;
use tokio::sync;
use tokio::time;
use std::fmt;
use std::task;
use std::future::Future;
use std::pin::Pin;
use log::{info};

#[derive(Debug,Clone,PartialEq)]
pub enum CtxErr {
    Timeout,
    Cancelled,
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

impl fmt::Display for CtxErr {
    fn fmt(&self, f : &mut fmt::Formatter) -> fmt::Result {
        return write!(f,"{:?}",self);
    }
}

impl std::error::Error for CtxErr {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> { return None }
}

struct Ctx_ {
    label : Option<String>,
    done : Eventual<CtxErr>,
    deadline : Option<time::Instant>,
    parent : Option<Arc<Ctx_>>,
    children : RwLock<Vec<Weak<Ctx_>>>,
}

impl std::fmt::Debug for Ctx {
    fn fmt(&self, f:&mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.parent.as_ref().map(|p|Ctx(p.clone()).fmt(f));
        self.0.label.as_ref().map(|l|f.write_fmt(format_args!("::{}",&l)));
        Ok(())
    }
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
            label: None,
            parent: None,
            deadline: None,
            done: Eventual::new(),
            children: RwLock::new(vec![]),
        }));
    }

    fn cancel(&self) {
        info!("cancel");
        if !self.0.done.set(CtxErr::Cancelled) { return; }
        for c in self.0.children.write().unwrap().split_off(0) {
            if let Some(c) = c.upgrade() {
                Ctx(c).cancel();
            }
        }
    }

    pub fn err(&self) -> Option<CtxErr> { self.0.done.get() }

    pub fn done(&self) -> impl Future<Output=CtxErr> {
        let x = self.clone();
        async move {
            match x.0.deadline {
                Some(d) => match time::timeout_at(d,x.0.done.wait()).await {
                    Err(_) => {
                        x.0.done.set(CtxErr::Timeout);
                        x.0.done.get().unwrap() // get(), because there can be a race condition on set().
                    }
                    Ok(e) => e,
                }
                None => x.0.done.wait().await,
            }
        }
    }

    pub async fn wrap<F,T>(&self,f:F) -> Result<T,CtxErr> where
        F : std::future::Future<Output=T>,
    {
        //info!("await {:?}",self);
        let res = tokio::select!{
            v = f => Ok(v),
            v = self.done() => Err(v),
        };
        //info!("await done {:?}",self);
        return res;
    }

    pub async fn wait_until(&self, deadline:time::Instant) -> Result<(),CtxErr> {
        self.wrap(time::sleep_until(deadline)).await
    }

    pub async fn wait(&self, duration:time::Duration) -> Result<(),CtxErr> {
        self.wrap(time::sleep(duration)).await
    }

    pub fn with_cancel(&self) -> (Ctx,impl Fn()->()) {
        let mut children = self.0.children.write().unwrap();
        let done = if let Some(_) = self.0.done.get() {
            self.0.done.clone()
        } else {
            Eventual::new()
        };
        let ctx = Ctx(Arc::new(Ctx_{
            label: None,
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
            label: None,
            parent: Some(self.0.clone()),
            done: self.0.done.clone(),
            deadline: Some(std::cmp::min(deadline,self.0.deadline.unwrap_or(deadline))),
            children: RwLock::new(vec![]),
        }));
        children.push(Arc::downgrade(&ctx.0));
        return ctx;
    }

    pub fn with_label(&self, label:&str) -> Ctx {
        let mut children = self.0.children.write().unwrap();
        let ctx = Ctx(Arc::new(Ctx_{
            label: Some(label.to_string()),
            parent: Some(self.0.clone()),
            done: self.0.done.clone(),
            deadline: self.0.deadline,
            children: RwLock::new(vec![]),
        }));
        children.push(Arc::downgrade(&ctx.0));
        return ctx;
    }

    pub fn with_timeout(&self, timeout : time::Duration) -> Ctx {
        return self.with_deadline(time::Instant::now()+timeout)
    }
}
