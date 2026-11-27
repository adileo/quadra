//! An S3 compatible bucket as a store (`object_store`), under a prefix. Its calls run on a
//! runtime of their own, so that they can be made from the engine's thread and from blocking
//! tasks alike.

use crate::{Blobs, Result, Signed};
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path;
use object_store::signer::{SignedUrlOptions, Signer};
use object_store::{Attribute, Attributes, ObjectStore, ObjectStoreExt, PutOptions, PutPayload};
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Spec {
    pub bucket: String,
    pub prefix: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub key: String,
    pub secret: String,
}

pub struct S3 {
    spec: Spec,
    store: Arc<AmazonS3>,
}

/// The runtime the calls to buckets run on: two threads, for the life of the process.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("quadra-blobs")
            .enable_all()
            .build()
            .expect("the runtime of the files")
    })
}

/// Runs a call to the bucket and waits for it.
fn run<T: Send + 'static>(f: impl Future<Output = T> + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    runtime().spawn(async move {
        let _ = tx.send(f.await);
    });
    rx.recv().expect("the runtime of the files stopped")
}

impl S3 {
    pub fn new(spec: Spec) -> Result<S3> {
        let mut b = AmazonS3Builder::new()
            .with_bucket_name(&spec.bucket)
            .with_region(spec.region.as_deref().unwrap_or("us-east-1"))
            .with_access_key_id(&spec.key)
            .with_secret_access_key(&spec.secret)
            .with_client_options(
                object_store::ClientOptions::new()
                    .with_timeout(Duration::from_secs(120))
                    .with_connect_timeout(Duration::from_secs(10)),
            );
        if let Some(e) = &spec.endpoint {
            // a bucket of another provider (Hetzner, R2, MinIO): in the path, as all of them
            // take it
            b = b
                .with_endpoint(e)
                .with_virtual_hosted_style_request(false)
                .with_allow_http(e.starts_with("http://"));
        }
        let store = b
            .build()
            .map_err(|e| format!("[files] bucket `{}`: {e}", spec.bucket))?;
        Ok(S3 {
            spec,
            store: Arc::new(store),
        })
    }

    fn location(&self, key: &str) -> Path {
        Path::from(self.path_of(key))
    }

    fn err(&self, what: &str, key: &str, e: impl std::fmt::Display) -> String {
        format!("{what} {}/{}: {e}", self.spec.bucket, self.path_of(key))
    }
}

impl Blobs for S3 {
    fn put(&self, key: &str, bytes: &[u8], ctype: &str, disposition: &str) -> Result<()> {
        let (store, at) = (self.store.clone(), self.location(key));
        let mut attributes = Attributes::new();
        if !ctype.is_empty() {
            attributes.insert(Attribute::ContentType, ctype.to_string().into());
        }
        if !disposition.is_empty() {
            attributes.insert(
                Attribute::ContentDisposition,
                disposition.to_string().into(),
            );
            // the key is its hash: what is there never changes
            attributes.insert(
                Attribute::CacheControl,
                "private, max-age=31536000, immutable".into(),
            );
        }
        let opts = PutOptions {
            attributes,
            ..Default::default()
        };
        let payload = PutPayload::from(bytes.to_vec());
        run(async move { store.put_opts(&at, payload, opts).await })
            .map(|_| ())
            .map_err(|e| self.err("put", key, e))
    }

    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let (store, at) = (self.store.clone(), self.location(key));
        let r = run(async move {
            match store.get(&at).await {
                Ok(r) => r.bytes().await.map(Some),
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(e),
            }
        });
        r.map(|b| b.map(|b| b.to_vec()))
            .map_err(|e| self.err("get", key, e))
    }

    fn head(&self, key: &str) -> Result<Option<u64>> {
        let (store, at) = (self.store.clone(), self.location(key));
        run(async move {
            match store.head(&at).await {
                Ok(m) => Ok(Some(m.size)),
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(e),
            }
        })
        .map_err(|e| self.err("head", key, e))
    }

    fn delete(&self, key: &str) -> Result<()> {
        let (store, at) = (self.store.clone(), self.location(key));
        run(async move {
            match store.delete(&at).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                Err(e) => Err(e),
            }
        })
        .map_err(|e| self.err("delete", key, e))
    }

    fn copy(&self, from: &str, to: &str) -> Result<()> {
        let (store, a, b) = (self.store.clone(), self.location(from), self.location(to));
        run(async move { store.copy(&a, &b).await }).map_err(|e| self.err("copy", from, e))
    }

    fn put_url(&self, key: &str, _size: u64, valid: Duration) -> Result<Signed> {
        let (store, at) = (self.store.clone(), self.location(key));
        // a presigned PUT cannot bind its length (the signer keeps `content-length` to
        // itself): the size is checked when the server reads the upload
        let url = run(async move {
            store
                .signed_url(object_store::signer::Method::PUT, &at, valid)
                .await
        })
        .map_err(|e| self.err("sign", key, e))?;
        Ok(Signed {
            method: "PUT".into(),
            url: url.to_string(),
            headers: Vec::new(),
        })
    }

    fn get_url(
        &self,
        key: &str,
        valid: Duration,
        ctype: &str,
        disposition: &str,
    ) -> Result<String> {
        let (store, at) = (self.store.clone(), self.location(key));
        let opts = SignedUrlOptions::default().with_query([
            ("response-content-type", ctype.to_string()),
            ("response-content-disposition", disposition.to_string()),
        ]);
        run(async move {
            store
                .signed_url_opts(object_store::signer::Method::GET, &at, valid, &opts)
                .await
        })
        .map(|u| u.to_string())
        .map_err(|e| self.err("sign", key, e))
    }

    fn path_of(&self, key: &str) -> String {
        if self.spec.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{key}", self.spec.prefix)
        }
    }
}
