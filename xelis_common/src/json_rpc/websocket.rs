use std::{
    sync::{
        atomic::{AtomicUsize, Ordering, AtomicBool},
        Arc
    },
    collections::HashMap,
    hash::Hash,
    marker::PhantomData,
    borrow::Cow, time::Duration
};

use anyhow::Error;
use futures_util::{StreamExt, stream::{SplitSink, SplitStream}, SinkExt};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{Value, json};
use tokio::{net::TcpStream, sync::{Mutex, oneshot, broadcast}, time::sleep};
use tokio_tungstenite::{WebSocketStream, MaybeTlsStream, connect_async, tungstenite::Message};
use log::{error, debug, trace};

use crate::api::SubscribeParams;

use super::{JSON_RPC_VERSION, JsonRPCError, JsonRPCResponse, JsonRPCResult};

// EventReceiver allows to get the event value parsed directly
pub struct EventReceiver<T: DeserializeOwned> {
    inner: broadcast::Receiver<Value>,
    _phantom: PhantomData<T>
}

impl<T: DeserializeOwned> EventReceiver<T> {
    pub fn new(inner: broadcast::Receiver<Value>) -> Self {
        Self {
            inner,
            _phantom: PhantomData
        }
    }

    pub async fn next(&mut self) -> Result<T, Error> {
        let value = self.inner.recv().await?;
        Ok(serde_json::from_value(value)?)
    }
}

// It is around a Arc to be shareable easily
// it has a tokio task running in background to handle all incoming messages
pub type WebSocketJsonRPCClient<E> = Arc<WebSocketJsonRPCClientImpl<E>>;

// A JSON-RPC Client over WebSocket protocol to support events
// It can be used in multi-thread safely because each request/response are linked using the id attribute.
pub struct WebSocketJsonRPCClientImpl<E: Serialize + Hash + Eq + Send + Sync + Clone + 'static> {
    ws: Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>,
    count: AtomicUsize,
    requests: Mutex<HashMap<usize, oneshot::Sender<JsonRPCResponse>>>,
    handler_by_id: Mutex<HashMap<usize, broadcast::Sender<Value>>>,
    events_to_id: Mutex<HashMap<E, usize>>,
    // websocket server address
    target: String,
    auto_reconnect: Mutex<Option<Duration>>,
    online: AtomicBool
}

pub const DEFAULT_AUTO_RECONNECT: Duration = Duration::from_secs(5);

impl<E: Serialize + Hash + Eq + Send + Sync + Clone + 'static> WebSocketJsonRPCClientImpl<E> {
    async fn connect_to(target: &String) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, JsonRPCError> {
        let (ws, response) = connect_async(target).await?;
        let status = response.status();
        if status.is_server_error() || status.is_client_error() {
            return Err(JsonRPCError::ConnectionError(status.to_string()));
        }

        Ok(ws)
    }

    pub async fn new(mut target: String) -> Result<WebSocketJsonRPCClient<E>, JsonRPCError> {
        if target.starts_with("https://") {
            target.replace_range(..8, "wss://");
        }
        else if target.starts_with("http://") {
            target.replace_range(..7, "ws://");
        }
        else if !target.starts_with("ws://") && !target.starts_with("wss://") {
            target.insert_str(0, "ws://");
        }

        let ws = Self::connect_to(&target).await?;
        
        let (write, read) = ws.split();
        let client = Arc::new(WebSocketJsonRPCClientImpl {
            ws: Mutex::new(write),
            count: AtomicUsize::new(0),
            requests: Mutex::new(HashMap::new()),
            handler_by_id: Mutex::new(HashMap::new()),
            events_to_id: Mutex::new(HashMap::new()),
            target,
            auto_reconnect: Mutex::new(Some(DEFAULT_AUTO_RECONNECT)),
            online: AtomicBool::new(true)
        });

        {
            let client = client.clone();
            tokio::spawn(async move {
                if let Err(e) = client.read(read).await {
                    error!("Error in the WebSocket client ioloop: {:?}", e);
                };
            });
        }

        Ok(client)
    }

    // Generate a new ID for a JSON-RPC request
    fn next_id(&self) -> usize {
        self.count.fetch_add(1, Ordering::SeqCst)
    }

    // Should the client try to reconnect to the server if the connection is lost
    pub async fn should_auto_reconnect(&self) -> bool {
        self.auto_reconnect.lock().await.is_some()
    }

    // Set if the client should try to reconnect to the server if the connection is lost
    pub async fn set_auto_reconnect(&self, duration: Option<Duration>) {
        let mut reconnect = self.auto_reconnect.lock().await;
        *reconnect = duration;
    }

    // Is the client online
    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::SeqCst)
    }

    // resubscribe to all events because of a reconnection
    async fn resubscribe_events(&self) -> Result<(), JsonRPCError> {
        let events = {
            let events = self.events_to_id.lock().await;
            events.clone()
        };
        for (event, id) in events {
            // Send it to the server
            if !self.send::<_, bool>("subscribe", Some(id), &SubscribeParams {
                notify: Cow::Borrowed(&event),
            }).await? {
                error!("Error while resubscribing to event with id {}", id);
            }
        }
        Ok(())
    }
    // Try to reconnect to the server
    async fn try_reconnect(self: &Arc<Self>) -> Option<SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>> {
        trace!("try reconnect");
        // We are not online anymore
        self.online.store(false, Ordering::SeqCst);

        // Check if we should reconnect
        let mut reconnect = {
            let reconnect = self.auto_reconnect.lock().await;
            reconnect.clone()
        };
        
        // Try to reconnect to the server
        while let Some(duration) = reconnect.as_ref() {
            sleep(*duration).await;
            debug!("Trying to reconnect to the server...");

            let ws = match Self::connect_to(&self.target).await {
                Ok(ws) => ws,
                Err(e) => {
                    debug!("Error while reconnecting to the server: {:?}", e);
                    reconnect = {
                        let reconnect = self.auto_reconnect.lock().await;
                        reconnect.clone()
                    };
                    continue;
                }
            };

            // We are connected again, set back everything
            let (write, read) = ws.split();
            {
                let mut ws = self.ws.lock().await;
                *ws = write;
            }

            // Register all events again
            {
                let client = self.clone();
                tokio::spawn(async move {
                    if let Err(e) = client.resubscribe_events().await {
                        error!("Error while resubscribing to events: {:?}", e);
                    }
                });
            }

            // We are online again
            self.online.store(true, Ordering::SeqCst);

            return Some(read)
        }

        None
    }

    // Clear all pending requests to notifier the caller that the connection is lost
    async fn clear_requests(&self) {
        let mut requests = self.requests.lock().await;
        requests.clear();
    }

    // Clear all events
    // Because they are all channels, they will returns error to the caller
    async fn clear_events(&self) {
        {
            let mut events = self.events_to_id.lock().await;
            events.clear();
        }
        {
            let mut handlers = self.handler_by_id.lock().await;
            handlers.clear();
        }
    }

    // Task running in background to handle every messages from the WebSocket server
    // This includes Events propagated and responses to JSON-RPC requests
    async fn read(self: Arc<Self>, mut read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>) -> Result<(), JsonRPCError> {
        while let Some(res) = read.next().await {
            let msg = match res {
                Ok(msg) => msg,
                Err(e) => {
                    // Try to reconnect to the server
                    debug!("Error while reading from the WebSocket: {:?}", e);
                    self.clear_requests().await;
                    if let Some(new_read) = self.try_reconnect().await {
                        read = new_read;
                    }
                    else {
                        error!("Error while reading from the WebSocket: {:?}", e);
                        self.clear_events().await;
                        break;
                    }
                    continue;
                }
            };

            match msg {
                Message::Text(text) => {
                    let response: JsonRPCResponse = serde_json::from_str(&text)?;
                    if let Some(id) = response.id {
                        // send the response to the requester if it matches the ID
                        {
                            let mut requests = self.requests.lock().await;
                            if let Some(sender) = requests.remove(&id) {
                                if let Err(e) = sender.send(response) {
                                    error!("Error sending response to the request: {:?}", e);
                                }
                                continue;
                            }
                        }

                        // Check if this ID corresponds to a event subscribed
                        {
                            let mut handlers = self.handler_by_id.lock().await;
                            if let Some(sender) = handlers.get_mut(&id) {
                                // Check that we still have someone who listen it
                                if sender.receiver_count() > 0 {
                                    if let Err(e) = sender.send(response.result.unwrap_or_default()) {
                                        error!("Error sending event to the request: {:?}", e);
                                    }
                                }
                            }
                        }
                    }
                },
                Message::Close(_) => {
                    break;
                },
                _ => {}
            }
        }

        Ok(())
    }

    // Call a method without parameters
    pub async fn call<R: DeserializeOwned>(&self, method: &str) -> JsonRPCResult<R> {
        self.send(method, None, &Value::Null).await
    }

    // Call a method with parameters
    pub async fn call_with<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: &P) -> JsonRPCResult<R> {
        self.send(method, None, params).await
    }

    // Verify if we already subscribed to this event or not
    pub async fn has_event(&self, event: &E) -> bool {
        let events = self.events_to_id.lock().await;
        events.contains_key(&event)
    }

    // Subscribe to an event
    pub async fn subscribe_event<T: DeserializeOwned>(&self, event: E) -> JsonRPCResult<EventReceiver<T>> {
        // Returns a Receiver for this event if already registered
        {
            let ids = self.events_to_id.lock().await;
            if let Some(id) = ids.get(&event) {
                let handlers = self.handler_by_id.lock().await;
                if let Some(sender) = handlers.get(id) {
                    return Ok(EventReceiver::new(sender.subscribe()));
                }
            }
        }

        // Generate the ID for this request
        let id = self.next_id();

        // Send it to the server
        self.send::<_, bool>("subscribe", Some(id), &SubscribeParams {
            notify: Cow::Borrowed(&event)
        }).await?;

        // Create a mapping from the event to the ID used for the request
        {
            let mut ids = self.events_to_id.lock().await;
            ids.insert(event, id);
        }

        // Create a channel to receive the event
        let (sender, receiver) = broadcast::channel(1);
        {
            let mut handlers = self.handler_by_id.lock().await;
            handlers.insert(id, sender);
        }

        Ok(EventReceiver::new(receiver))
    }

    // Unsubscribe from an event
    pub async fn unsubscribe_event(&self, event: &E) -> JsonRPCResult<()> {        
        // Retrieve the id for this event
        let id = {
            let mut ids = self.events_to_id.lock().await;
            ids.remove(event).ok_or(JsonRPCError::EventNotRegistered)?
        };

        // Send the unsubscribe rpc method
        self.send::<E, bool>("unsubscribe", None, event).await?;

        // delete it from events list
        {
            let mut handlers = self.handler_by_id.lock().await;
            handlers.remove(&id);
        }

        Ok(())
    }

    async fn send_message_internal<P: Serialize>(&self, id: usize, method: &str, params: &P) -> JsonRPCResult<()> {
        let mut ws = self.ws.lock().await;
        ws.send(Message::Text(serde_json::to_string(&json!({
            "jsonrpc": JSON_RPC_VERSION,
            "method": method,
            "id": id,
            "params": params
        }))?)).await?;

        Ok(())
    }

    // Send a request to the server and wait for the response
    async fn send<P: Serialize, R: DeserializeOwned>(&self, method: &str, id: Option<usize>, params: &P) -> JsonRPCResult<R> {
        let id = id.unwrap_or_else(|| self.next_id());
        let (sender, receiver) = oneshot::channel();
        {
            let mut requests = self.requests.lock().await;
            requests.insert(id, sender);
        }

        self.send_message_internal(id, method, params).await?;

        let response = receiver.await.or(Err(JsonRPCError::NoResponse))?;
        if let Some(error) = response.error {
            return Err(JsonRPCError::ServerError {
                code: error.code,
                message: error.message,
                data: error.data.map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
            });
        }

        let result = response.result.ok_or(JsonRPCError::NoResponse)?;

        Ok(serde_json::from_value(result)?)
    }
}