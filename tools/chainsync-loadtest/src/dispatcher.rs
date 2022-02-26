use std::sync::{Arc,Weak,Mutex,RwLock};
use tokio::sync;
use tokio::time;
use std::fmt;
use core::task;
use core::future::Future;
use core::pin::Pin;
use std::cmp::Eq;
use std::hash::Hash;
use std::fmt::Debug;
use log::{error, info, warn};

struct Dispatcher_<K:Hash+Eq+Clone,V:Clone> {
    receivers: std::collections::HashMap<K,Vec<Weak<Mutex<Receiver_<K,V>>>>>,
}

struct Receiver_<K:Hash+Eq+Clone,V:Clone> {
    key : K,
    value : Option<V>,
    disp : Arc<Mutex<Dispatcher_<K,V>>>,
    waker : Option<task::Waker>,
}

pub struct Dispatcher<K:Hash+Eq+Clone,V:Clone>(Arc<Mutex<Dispatcher_<K,V>>>);

pub struct Receiver<K:Hash+Eq+Clone,V:Clone>(Arc<Mutex<Receiver_<K,V>>>);

impl<K:Hash+Eq+Clone,V:Clone> Default for Dispatcher<K,V> {
    fn default() -> Dispatcher<K,V> {
        return Dispatcher(Arc::new(Mutex::new(Dispatcher_{
            receivers: Default::default(),
        })));
    }
}

impl<K:Hash+Eq+Clone,V:Clone> Dispatcher<K,V> {
    pub fn send(&self, k:&K, v:V) { (||{
        let mut d = self.0.lock().unwrap();
        for r in d.receivers.remove(k)? {
            let r = r.upgrade();
            r.map(|r|{
                let mut r = r.lock().unwrap();
                r.value = Some(v.clone());
                r.waker.take().map(|w|w.wake());
            });
        }
        Some(())
    })(); }

    fn flush(&self,k:&K) {
        let mut d = self.0.lock().unwrap();
        d.receivers.get_mut(k).map(|v|v.retain(|w|w.upgrade().is_some()));
    }

    pub fn subscribe(&self, k:&K) -> Receiver<K,V> {
        let r = Arc::new(Mutex::new(Receiver_{
            key: k.clone(),
            value: None,
            disp: self.0.clone(),
            waker: None,
        }));
        let mut d = self.0.lock().unwrap();
        d.receivers.entry(k.clone()).or_default().push(Arc::downgrade(&r));
        return Receiver(r);
    }
}

impl<K:Hash+Eq+Clone,V:Clone> Drop for Receiver_<K,V> {
    fn drop(&mut self) {
        Dispatcher(self.disp.clone()).flush(&self.key);
    }
}

impl<K:Hash+Eq+Clone,V:Clone> Future for Receiver<K,V> {
    type Output = V;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> task::Poll<V> {
        let mut w = self.0.lock().unwrap();
        match w.value.take() {
            Some(v) => task::Poll::Ready(v),
            None => {
                w.waker = Some(cx.waker().clone());
                task::Poll::Pending
            },
        }
    }
}

