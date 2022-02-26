struct Scope_ {
    cancel : Option<Box<dyn FnOnce() -> () + Send>>,
    err : Option<anyhow::Error>, 
    futures : Vec<tokio::task::JoinHandle<()>>,
}

pub struct Scope {
    ctx : Ctx,
    data : Mutex<Scope_>,
}

impl Scope_ {
    fn complete(&mut self, v:anyhow::Result<()>) {
        if let Err(e) = v {
            if let Some(cancel) = self.cancel.take() {
                cancel();
                self.err = Some(e);
            }
        }
    }
}

impl Scope {
    fn complete(&self, v:anyhow::Result<()>) {
        return self.data.lock().unwrap().complete(v);
    }

    pub fn spawn<F>(self :&Arc<Self>, f:impl Send + FnOnce(Ctx)->F) where
        F:Future<Output=anyhow::Result<()>> + Send + 'static,
    {
        let s = self.clone();
        let fut = f(s.ctx.clone());
        self.data.lock().unwrap().futures.push(tokio::spawn(async move {
            s.complete(fut.await);
        }));
    }
}

pub async fn scope<F>(ctx :&Ctx, f:impl FnOnce(Ctx,Arc<Scope>) -> F) -> anyhow::Result<()> where
    F : Future<Output=anyhow::Result<()>> + Send + 'static
{
    let (ctx,cancel) = self.with_cancel();
    let s = Arc::new(Scope{
        ctx:ctx.clone(),
        data: Mutex::new(Scope_{
            cancel:Some(Box::new(cancel)),
            err:None,
            futures: vec![],
        }),
    });
    s.complete(f(ctx,s.clone()).await);
    
    loop {
        let fut = s.data.lock().unwrap().futures.pop();
        match fut {
            None => { return match s.data.lock().unwrap().err.take() {
                None => Ok(()),
                Some(e) => Err(e),
            }; }
            Some(fut) => { fut.await.unwrap(); }
        }
    }
}
