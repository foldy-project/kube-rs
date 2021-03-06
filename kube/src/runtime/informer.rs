use crate::{
    api::{ListParams, Meta, Resource, WatchEvent},
    Client, Result,
};

use futures::{lock::Mutex, Stream, StreamExt};
use serde::de::DeserializeOwned;
use std::{sync::Arc, time::Duration};
use tokio::time::delay_for;

/// An event informer for a `Resource`
///
/// This watches a `Resource<K>`, by:
/// - seeding the intial resourceVersion with a list call (optional)
/// - keeping track of resourceVersions during every poll
/// - recovering when resourceVersions get desynced
#[derive(Clone)]
pub struct Informer<K>
where
    K: Clone + DeserializeOwned + Meta,
{
    version: Arc<Mutex<String>>,
    client: Client,
    resource: Resource,
    params: ListParams,
    needs_resync: Arc<Mutex<bool>>,
    needs_retry: Arc<Mutex<bool>>,
    phantom: std::marker::PhantomData<K>,
}

impl<K> Informer<K>
where
    K: Clone + DeserializeOwned + Meta,
{
    /// Create a reflector with a kube client on a kube resource
    pub fn new(client: Client, lp: ListParams, r: Resource) -> Self {
        Informer {
            client,
            resource: r,
            params: lp,
            version: Arc::new(Mutex::new(0.to_string())),
            needs_resync: Arc::new(Mutex::new(false)),
            needs_retry: Arc::new(Mutex::new(false)),
            phantom: std::marker::PhantomData,
        }
    }

    /// Initialize from a prior version
    pub fn init_from(self, v: String) -> Self {
        info!("Recreating Informer for {} at {}", self.resource.kind, v);

        // We need to block on this as our mutex needs go be async compatible
        futures::executor::block_on(async {
            *self.version.lock().await = v;
        });
        self
    }

    /// Start a single watch stream
    ///
    /// Opens a long polling GET and returns the complete WatchEvents as a Stream.
    /// You should always poll. When this call ends, call it again.
    /// Do not call it from more than one context.
    ///
    /// This function will handle error handling up to a point:
    /// - if we go out of history (410 Gone), we reset to latest
    /// - if we failed an initial poll, we will retry
    /// All real errors are bubbled up, as are WachEvent::Error instances.
    /// In the retry/reset cases we wait 10s between each attempt.
    ///
    /// If you need to track the `resourceVersion` you can use `Informer::version()`.
    pub async fn poll(&self) -> Result<impl Stream<Item = Result<WatchEvent<K>>>> {
        trace!("Watching {}", self.resource.kind);

        // First check if we need to backoff or reset our resourceVersion from last time
        {
            let mut needs_retry = self.needs_retry.lock().await;
            let mut needs_resync = self.needs_resync.lock().await;
            if *needs_resync || *needs_retry {
                // Try again in a bit
                let dur = Duration::from_secs(10);
                delay_for(dur).await;
                // If we are outside history, start over from latest
                if *needs_resync {
                    self.reset().await;
                }
                *needs_resync = false;
                *needs_retry = false;
            }
        }

        // Create our watch request
        let resource_version = self.version.lock().await.clone();
        let req = self.resource.watch(&self.params, &resource_version)?;

        // Clone Arcs for stream handling
        let version = self.version.clone();
        let needs_resync = self.needs_resync.clone();

        // Attempt to fetch our stream
        let stream = self.client.request_events::<WatchEvent<K>>(req).await;

        match stream {
            Ok(events) => {
                // Intercept stream elements to update internal resourceVersion
                Ok(events.then(move |event| {
                    // Need to clone our Arcs as they are consumed each loop
                    let needs_resync = needs_resync.clone();
                    let version = version.clone();
                    async move {
                        // Check if we need to update our version based on the incoming events
                        match &event {
                            Ok(WatchEvent::Added(o))
                            | Ok(WatchEvent::Modified(o))
                            | Ok(WatchEvent::Deleted(o)) => {
                                // Follow docs conventions and store the last resourceVersion
                                // https://kubernetes.io/docs/reference/using-api/api-concepts/#efficient-detection-of-changes
                                if let Some(nv) = Meta::resource_ver(o) {
                                    *version.lock().await = nv.clone();
                                }
                            }
                            Ok(WatchEvent::Error(e)) => {
                                // 410 Gone => we need to restart from latest next call
                                if e.code == 410 {
                                    warn!("Stream desynced: {:?}", e);
                                    *needs_resync.lock().await = true;
                                }
                            }
                            Err(e) => {
                                warn!("Unexpected watch error: {:?}", e);
                            }
                        };
                        event
                    }
                }))
            }
            Err(e) => {
                warn!("Poll error: {:?}", e);
                // If we failed to do the main watch - try again later with same version
                *self.needs_retry.lock().await = false;
                Err(e)
            }
        }
    }

    /// Reset the resourceVersion to 0
    ///
    /// Note: This will cause duplicate Added events for all existing resources
    pub async fn reset(&self) {
        *self.version.lock().await = 0.to_string();
    }

    /// Return the current version
    pub fn version(&self) -> String {
        // We need to block on a future here quickly
        // to get a lock on our version
        futures::executor::block_on(async { self.version.lock().await.clone() })
    }
}
