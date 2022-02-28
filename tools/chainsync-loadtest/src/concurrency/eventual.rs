use std::task;
use std::future::Future;
use std::sync::{Arc,Weak,RwLock};
use std::pin::Pin;

struct Eventual_<T:Clone> {
    value:Option<T>,
    waiters: Vec<Weak<RwLock<Waiter_<T>>>>,
}

struct Waiter_<T:Clone> {
    waker:Option<task::Waker>,
    e:Arc<RwLock<Eventual_<T>>>,
}

#[derive(Clone)]
pub struct Eventual<T:Clone>(Arc<RwLock<Eventual_<T>>>);

pub struct Waiter<T:Clone>(Arc<RwLock<Waiter_<T>>>);

impl<T:Clone> Eventual<T> {
    pub fn new() -> Eventual<T> {
        return Eventual(Arc::new(RwLock::new(Eventual_{
            value: None,
            waiters: vec![],
        }))); 
    }

    pub fn set(&self, v : T) -> bool {
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
   
    pub fn get(&self) -> Option<T> {
        return self.0.read().unwrap().value.clone();
    }

    pub fn wait(&self) -> Waiter<T> {
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
