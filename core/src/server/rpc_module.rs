// Copyright 2019-2021 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any
// person obtaining a copy of this software and associated
// documentation files (the "Software"), to deal in the
// Software without restriction, including without
// limitation the rights to use, copy, modify, merge,
// publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software
// is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice
// shall be included in all copies or substantial portions
// of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use std::collections::hash_map::Entry;
use std::fmt::{self, Debug};
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use crate::error::{Error, SubscriptionClosed, SubscriptionClosedReason};
use crate::id_providers::RandomIntegerIdProvider;
use crate::server::helpers::MethodSink;
use crate::server::resource_limiting::{ResourceGuard, ResourceTable, ResourceVec, Resources};
use crate::traits::{IdProvider, ToRpcParams};
use futures_channel::{mpsc, oneshot};
use futures_util::future::Either;
use futures_util::pin_mut;
use futures_util::{future::BoxFuture, FutureExt, Stream, StreamExt};
use jsonrpsee_types::error::{ErrorCode, CALL_EXECUTION_FAILED_CODE};
use jsonrpsee_types::{
	Id, Params, Request, Response, SubscriptionId as RpcSubscriptionId, SubscriptionPayload, SubscriptionResponse,
};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::Notify;

/// A `MethodCallback` is an RPC endpoint, callable with a standard JSON-RPC request,
/// implemented as a function pointer to a `Fn` function taking four arguments:
/// the `id`, `params`, a channel the function uses to communicate the result (or error)
/// back to `jsonrpsee`, and the connection ID (useful for the websocket transport).
pub type SyncMethod = Arc<dyn Send + Sync + Fn(Id, Params, &MethodSink) -> bool>;
/// Similar to [`SyncMethod`], but represents an asynchronous handler and takes an additional argument containing a [`ResourceGuard`] if configured.
pub type AsyncMethod<'a> = Arc<
	dyn Send + Sync + Fn(Id<'a>, Params<'a>, MethodSink, ConnectionId, Option<ResourceGuard>) -> BoxFuture<'a, bool>,
>;
/// Method callback for subscriptions.
pub type SubscriptionMethod = Arc<dyn Send + Sync + Fn(Id, Params, &MethodSink, ConnState) -> bool>;

/// Connection ID, used for stateful protocol such as WebSockets.
/// For stateless protocols such as http it's unused, so feel free to set it some hardcoded value.
pub type ConnectionId = usize;

/// Raw response from an RPC
/// A 3-tuple containing:
///   - Call result as a `String`,
///   - a [`mpsc::UnboundedReceiver<String>`] to receive future subscription results
///   - a [`tokio::sync::Notify`] to allow subscribers to notify their [`SubscriptionSink`] when they disconnect.
pub type RawRpcResponse = (String, mpsc::UnboundedReceiver<String>, Arc<Notify>);

/// Helper struct to manage subscriptions.
pub struct ConnState<'a> {
	/// Connection ID
	pub conn_id: ConnectionId,
	/// Get notified when the connection to subscribers is closed.
	pub close_notify: Arc<Notify>,
	/// ID provider.
	pub id_provider: &'a dyn IdProvider,
}

impl<'a> std::fmt::Debug for ConnState<'a> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("ConnState").field("conn_id", &self.conn_id).field("close", &self.close_notify).finish()
	}
}

type Subscribers = Arc<Mutex<FxHashMap<SubscriptionKey, (MethodSink, oneshot::Receiver<()>)>>>;

/// Represent a unique subscription entry based on [`RpcSubscriptionId`] and [`ConnectionId`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SubscriptionKey {
	conn_id: ConnectionId,
	sub_id: RpcSubscriptionId<'static>,
}

/// Callback wrapper that can be either sync or async.
#[derive(Clone)]
pub enum MethodKind {
	/// Synchronous method handler.
	Sync(SyncMethod),
	/// Asynchronous method handler.
	Async(AsyncMethod<'static>),
	/// Subscription method handler
	Subscription(SubscriptionMethod),
}

/// Information about resources the method uses during its execution. Initialized when the the server starts.
#[derive(Clone, Debug)]
enum MethodResources {
	/// Uninitialized resource table, mapping string label to units.
	Uninitialized(Box<[(&'static str, u16)]>),
	/// Initialized resource table containing units for each `ResourceId`.
	Initialized(ResourceTable),
}

/// Method callback wrapper that contains a sync or async closure,
/// plus a table with resources it needs to claim to run
#[derive(Clone, Debug)]
pub struct MethodCallback {
	callback: MethodKind,
	resources: MethodResources,
}

/// Result of a method, either direct value or a future of one.
pub enum MethodResult<T> {
	/// Result by value
	Sync(T),
	/// Future of a value
	Async(BoxFuture<'static, T>),
}

impl<T: Debug> Debug for MethodResult<T> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self {
			MethodResult::Sync(result) => result.fmt(f),
			MethodResult::Async(_) => f.write_str("<future>"),
		}
	}
}

/// Builder for configuring resources used by a method.
#[derive(Debug)]
pub struct MethodResourcesBuilder<'a> {
	build: ResourceVec<(&'static str, u16)>,
	callback: &'a mut MethodCallback,
}

impl<'a> MethodResourcesBuilder<'a> {
	/// Define how many units of a given named resource the method uses during its execution.
	pub fn resource(mut self, label: &'static str, units: u16) -> Result<Self, Error> {
		self.build.try_push((label, units)).map_err(|_| Error::MaxResourcesReached)?;
		Ok(self)
	}
}

impl<'a> Drop for MethodResourcesBuilder<'a> {
	fn drop(&mut self) {
		self.callback.resources = MethodResources::Uninitialized(self.build[..].into());
	}
}

impl MethodCallback {
	fn new_sync(callback: SyncMethod) -> Self {
		MethodCallback { callback: MethodKind::Sync(callback), resources: MethodResources::Uninitialized([].into()) }
	}

	fn new_async(callback: AsyncMethod<'static>) -> Self {
		MethodCallback { callback: MethodKind::Async(callback), resources: MethodResources::Uninitialized([].into()) }
	}

	fn new_subscription(callback: SubscriptionMethod) -> Self {
		MethodCallback {
			callback: MethodKind::Subscription(callback),
			resources: MethodResources::Uninitialized([].into()),
		}
	}

	/// Attempt to claim resources prior to executing a method. On success returns a guard that releases
	/// claimed resources when dropped.
	pub fn claim(&self, name: &str, resources: &Resources) -> Result<ResourceGuard, Error> {
		match self.resources {
			MethodResources::Uninitialized(_) => Err(Error::UninitializedMethod(name.into())),
			MethodResources::Initialized(units) => resources.claim(units),
		}
	}

	/// Get handle to the callback.
	pub fn inner(&self) -> &MethodKind {
		&self.callback
	}
}

impl Debug for MethodKind {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Async(_) => write!(f, "Async"),
			Self::Sync(_) => write!(f, "Sync"),
			Self::Subscription(_) => write!(f, "Subscription"),
		}
	}
}

/// Reference-counted, clone-on-write collection of synchronous and asynchronous methods.
#[derive(Default, Debug, Clone)]
pub struct Methods {
	callbacks: Arc<FxHashMap<&'static str, MethodCallback>>,
}

impl Methods {
	/// Creates a new empty [`Methods`].
	pub fn new() -> Self {
		Self::default()
	}

	fn verify_method_name(&mut self, name: &'static str) -> Result<(), Error> {
		if self.callbacks.contains_key(name) {
			return Err(Error::MethodAlreadyRegistered(name.into()));
		}

		Ok(())
	}

	/// Inserts the method callback for a given name, or returns an error if the name was already taken.
	/// On success it returns a mut reference to the [`MethodCallback`] just inserted.
	fn verify_and_insert(
		&mut self,
		name: &'static str,
		callback: MethodCallback,
	) -> Result<&mut MethodCallback, Error> {
		match self.mut_callbacks().entry(name) {
			Entry::Occupied(_) => Err(Error::MethodAlreadyRegistered(name.into())),
			Entry::Vacant(vacant) => Ok(vacant.insert(callback)),
		}
	}

	/// Initialize resources for all methods in this collection. This method has no effect if called more than once.
	pub fn initialize_resources(mut self, resources: &Resources) -> Result<Self, Error> {
		let callbacks = self.mut_callbacks();

		for (&method_name, callback) in callbacks.iter_mut() {
			if let MethodResources::Uninitialized(uninit) = &callback.resources {
				let mut map = resources.defaults;

				for &(label, units) in uninit.iter() {
					let idx = match resources.labels.iter().position(|&l| l == label) {
						Some(idx) => idx,
						None => return Err(Error::ResourceNameNotFoundForMethod(label, method_name)),
					};

					// If resource capacity set to `0`, we ignore the unit value of the method
					// and set it to `0` as well, effectively making the resource unlimited.
					if resources.capacities[idx] == 0 {
						map[idx] = 0;
					} else {
						map[idx] = units;
					}
				}

				callback.resources = MethodResources::Initialized(map);
			}
		}

		Ok(self)
	}

	/// Helper for obtaining a mut ref to the callbacks HashMap.
	fn mut_callbacks(&mut self) -> &mut FxHashMap<&'static str, MethodCallback> {
		Arc::make_mut(&mut self.callbacks)
	}

	/// Merge two [`Methods`]'s by adding all [`MethodCallback`]s from `other` into `self`.
	/// Fails if any of the methods in `other` is present already.
	pub fn merge(&mut self, other: impl Into<Methods>) -> Result<(), Error> {
		let mut other = other.into();

		for name in other.callbacks.keys() {
			self.verify_method_name(name)?;
		}

		let callbacks = self.mut_callbacks();

		for (name, callback) in other.mut_callbacks().drain() {
			callbacks.insert(name, callback);
		}

		Ok(())
	}

	/// Returns the method callback.
	pub fn method(&self, method_name: &str) -> Option<&MethodCallback> {
		self.callbacks.get(method_name)
	}

	/// Returns the method callback along with its name. The returned name is same as the
	/// `method_name`, but its lifetime bound is `'static`.
	pub fn method_with_name(&self, method_name: &str) -> Option<(&'static str, &MethodCallback)> {
		self.callbacks.get_key_value(method_name).map(|(k, v)| (*k, v))
	}

	/// Helper to call a method on the `RPC module` without having to spin up a server.
	///
	/// The params must be serializable as JSON array, see [`ToRpcParams`] for further documentation.
	///
	/// Returns the decoded value of the `result field` in JSON-RPC response if succesful.
	///
	/// # Examples
	///
	/// ```
	/// #[tokio::main]
	/// async fn main() {
	///     use jsonrpsee::RpcModule;
	///
	///     let mut module = RpcModule::new(());
	///     module.register_method("echo_call", |params, _| {
	///         params.one::<u64>().map_err(Into::into)
	///     }).unwrap();
	///
	///     let echo: u64 = module.call("echo_call", [1_u64]).await.unwrap();
	///     assert_eq!(echo, 1);
	/// }
	/// ```
	pub async fn call<Params: ToRpcParams, T: DeserializeOwned>(
		&self,
		method: &str,
		params: Params,
	) -> Result<T, Error> {
		let params = params.to_rpc_params()?;
		let req = Request::new(method.into(), Some(&params), Id::Number(0));
		tracing::trace!("[Methods::call] Calling method: {:?}, params: {:?}", method, params);
		let (resp, _, _) = self.inner_call(req).await;
		if let Ok(res) = serde_json::from_str::<Response<T>>(&resp) {
			return Ok(res.result);
		}
		Err(Error::Request(resp))
	}

	/// Make a request (JSON-RPC method call or subscription) by using raw JSON.
	///
	/// Returns the raw JSON response to the call and a stream to receive notifications if the call was a subscription.
	///
	/// # Examples
	///
	/// ```
	/// #[tokio::main]
	/// async fn main() {
	///     use jsonrpsee::RpcModule;
	///     use jsonrpsee::types::Response;
	///     use futures_util::StreamExt;
	///
	///     let mut module = RpcModule::new(());
	///     module.register_subscription("hi", "hi", "goodbye", |_, mut sink, _| {
	///         sink.send(&"one answer").unwrap();
	///         Ok(())
	///     }).unwrap();
	///     let (resp, mut stream) = module.raw_json_request(r#"{"jsonrpc":"2.0","method":"hi","id":0}"#).await.unwrap();
	///     let resp = serde_json::from_str::<Response<u64>>(&resp).unwrap();
	///     let sub_resp = stream.next().await.unwrap();
	///     assert_eq!(
	///         format!(r#"{{"jsonrpc":"2.0","method":"hi","params":{{"subscription":{},"result":"one answer"}}}}"#, resp.result),
	///         sub_resp
	///     );
	/// }
	/// ```
	pub async fn raw_json_request(&self, call: &str) -> Result<(String, mpsc::UnboundedReceiver<String>), Error> {
		tracing::trace!("[Methods::raw_json_request] {:?}", call);
		let req: Request = serde_json::from_str(call)?;
		let (resp, rx, _) = self.inner_call(req).await;
		Ok((resp, rx))
	}

	/// Execute a callback.
	async fn inner_call(&self, req: Request<'_>) -> RawRpcResponse {
		let (tx_sink, mut rx_sink) = mpsc::unbounded();
		let sink = MethodSink::new(tx_sink);
		let id = req.id.clone();
		let params = Params::new(req.params.map(|params| params.get()));
		let notify = Arc::new(Notify::new());

		let _result = match self.method(&req.method).map(|c| &c.callback) {
			None => sink.send_error(req.id, ErrorCode::MethodNotFound.into()),
			Some(MethodKind::Sync(cb)) => (cb)(id, params, &sink),
			Some(MethodKind::Async(cb)) => (cb)(id.into_owned(), params.into_owned(), sink, 0, None).await,
			Some(MethodKind::Subscription(cb)) => {
				let close_notify = notify.clone();
				let conn_state = ConnState { conn_id: 0, close_notify, id_provider: &RandomIntegerIdProvider };
				(cb)(id, params, &sink, conn_state)
			}
		};

		let resp = rx_sink.next().await.expect("tx and rx still alive; qed");

		(resp, rx_sink, notify)
	}

	/// Helper to create a subscription on the `RPC module` without having to spin up a server.
	///
	/// The params must be serializable as JSON array, see [`ToRpcParams`] for further documentation.
	///
	/// Returns [`Subscription`] on succes which can used to get results from the subscriptions.
	///
	/// # Examples
	///
	/// ```
	/// #[tokio::main]
	/// async fn main() {
	///     use jsonrpsee::{RpcModule, types::EmptyParams};
	///
	///     let mut module = RpcModule::new(());
	///     module.register_subscription("hi", "hi", "goodbye", |_, mut sink, _| {
	///         sink.send(&"one answer").unwrap();
	///         Ok(())
	///     }).unwrap();
	///
	///     let mut sub = module.subscribe("hi", EmptyParams::new()).await.unwrap();
	///     // In this case we ignore the subscription ID,
	///     let (sub_resp, _sub_id) = sub.next::<String>().await.unwrap().unwrap();
	///     assert_eq!(&sub_resp, "one answer");
	/// }
	/// ```
	pub async fn subscribe(&self, sub_method: &str, params: impl ToRpcParams) -> Result<Subscription, Error> {
		let params = params.to_rpc_params()?;
		let req = Request::new(sub_method.into(), Some(&params), Id::Number(0));
		tracing::trace!("[Methods::subscribe] Calling subscription method: {:?}, params: {:?}", sub_method, params);
		let (response, rx, close_notify) = self.inner_call(req).await;
		let subscription_response = serde_json::from_str::<Response<RpcSubscriptionId>>(&response)?;
		let sub_id = subscription_response.result.into_owned();
		let close_notify = Some(close_notify);
		Ok(Subscription { sub_id, rx, close_notify })
	}

	/// Returns an `Iterator` with all the method names registered on this server.
	pub fn method_names(&self) -> impl Iterator<Item = &'static str> + '_ {
		self.callbacks.keys().copied()
	}
}

impl<Context> Deref for RpcModule<Context> {
	type Target = Methods;

	fn deref(&self) -> &Methods {
		&self.methods
	}
}

impl<Context> DerefMut for RpcModule<Context> {
	fn deref_mut(&mut self) -> &mut Methods {
		&mut self.methods
	}
}

/// Sets of JSON-RPC methods can be organized into a "module"s that are in turn registered on the server or,
/// alternatively, merged with other modules to construct a cohesive API. [`RpcModule`] wraps an additional context
/// argument that can be used to access data during call execution.
#[derive(Debug, Clone)]
pub struct RpcModule<Context> {
	ctx: Arc<Context>,
	methods: Methods,
}

impl<Context> RpcModule<Context> {
	/// Create a new module with a given shared `Context`.
	pub fn new(ctx: Context) -> Self {
		Self { ctx: Arc::new(ctx), methods: Default::default() }
	}

	/// Transform a module into an `RpcModule<()>` (unit context).
	pub fn remove_context(self) -> RpcModule<()> {
		let mut module = RpcModule::new(());
		module.methods = self.methods;
		module
	}
}

impl<Context> From<RpcModule<Context>> for Methods {
	fn from(module: RpcModule<Context>) -> Methods {
		module.methods
	}
}

impl<Context: Send + Sync + 'static> RpcModule<Context> {
	/// Register a new synchronous RPC method, which computes the response with the given callback.
	pub fn register_method<R, F>(
		&mut self,
		method_name: &'static str,
		callback: F,
	) -> Result<MethodResourcesBuilder, Error>
	where
		Context: Send + Sync + 'static,
		R: Serialize,
		F: Fn(Params, &Context) -> Result<R, Error> + Send + Sync + 'static,
	{
		let ctx = self.ctx.clone();
		let callback = self.methods.verify_and_insert(
			method_name,
			MethodCallback::new_sync(Arc::new(move |id, params, sink| match callback(params, &*ctx) {
				Ok(res) => sink.send_response(id, res),
				Err(err) => sink.send_call_error(id, err),
			})),
		)?;

		Ok(MethodResourcesBuilder { build: ResourceVec::new(), callback })
	}

	/// Register a new asynchronous RPC method, which computes the response with the given callback.
	pub fn register_async_method<R, Fun, Fut>(
		&mut self,
		method_name: &'static str,
		callback: Fun,
	) -> Result<MethodResourcesBuilder, Error>
	where
		R: Serialize + Send + Sync + 'static,
		Fut: Future<Output = Result<R, Error>> + Send,
		Fun: (Fn(Params<'static>, Arc<Context>) -> Fut) + Copy + Send + Sync + 'static,
	{
		let ctx = self.ctx.clone();
		let callback = self.methods.verify_and_insert(
			method_name,
			MethodCallback::new_async(Arc::new(move |id, params, sink, _, claimed| {
				let ctx = ctx.clone();
				let future = async move {
					let result = match callback(params, ctx).await {
						Ok(res) => sink.send_response(id, res),
						Err(err) => sink.send_call_error(id, err),
					};

					// Release claimed resources
					drop(claimed);

					result
				};
				future.boxed()
			})),
		)?;

		Ok(MethodResourcesBuilder { build: ResourceVec::new(), callback })
	}

	/// Register a new **blocking** synchronous RPC method, which computes the response with the given callback.
	/// Unlike the regular [`register_method`](RpcModule::register_method), this method can block its thread and perform expensive computations.
	pub fn register_blocking_method<R, F>(
		&mut self,
		method_name: &'static str,
		callback: F,
	) -> Result<MethodResourcesBuilder, Error>
	where
		Context: Send + Sync + 'static,
		R: Serialize,
		F: Fn(Params, Arc<Context>) -> Result<R, Error> + Copy + Send + Sync + 'static,
	{
		let ctx = self.ctx.clone();
		let callback = self.methods.verify_and_insert(
			method_name,
			MethodCallback::new_async(Arc::new(move |id, params, sink, _, claimed| {
				let ctx = ctx.clone();

				tokio::task::spawn_blocking(move || {
					let result = match callback(params, ctx) {
						Ok(res) => sink.send_response(id, res),
						Err(err) => sink.send_call_error(id, err),
					};

					// Release claimed resources
					drop(claimed);

					result
				})
				.map(|result| match result {
					Ok(r) => r,
					Err(err) => {
						tracing::error!("Join error for blocking RPC method: {:?}", err);
						false
					}
				})
				.boxed()
			})),
		)?;

		Ok(MethodResourcesBuilder { build: ResourceVec::new(), callback })
	}

	/// Register a new publish/subscribe interface using JSON-RPC notifications.
	///
	/// It implements the [ethereum pubsub specification](https://geth.ethereum.org/docs/rpc/pubsub)
	/// with an option to choose custom subscription ID generation.
	///
	/// Furthermore, it generates the `unsubscribe implementation` where a `bool` is used as
	/// the result to indicate whether the subscription was successfully unsubscribed to or not.
	/// For instance an `unsubscribe call` may fail if a non-existent subscriptionID is used in the call.
	///
	/// This method ensures that the `subscription_method_name` and `unsubscription_method_name` are unique.
	/// The `notif_method_name` argument sets the content of the `method` field in the JSON document that
	/// the server sends back to the client. The uniqueness of this value is not machine checked and it's up to
	/// the user to ensure it is not used in any other [`RpcModule`] used in the server.
	///
	/// # Arguments
	///
	/// * `subscription_method_name` - name of the method to call to initiate a subscription
	/// * `notif_method_name` - name of method to be used in the subscription payload (technically a JSON-RPC notification)
	/// * `unsubscription_method` - name of the method to call to terminate a subscription
	/// * `callback` - A callback to invoke on each subscription; it takes three parameters:
	///     - [`Params`]: JSON-RPC parameters in the subscription call.
	///     - [`SubscriptionSink`]: A sink to send messages to the subscriber.
	///     - Context: Any type that can be embedded into the [`RpcModule`].
	///
	/// # Examples
	///
	/// ```no_run
	///
	/// use jsonrpsee_core::server::rpc_module::RpcModule;
	///
	/// let mut ctx = RpcModule::new(99_usize);
	/// ctx.register_subscription("sub", "notif_name", "unsub", |params, mut sink, ctx| {
	///     let x: usize = params.one()?;
	///     std::thread::spawn(move || {
	///         let sum = x + (*ctx);
	///         sink.send(&sum)
	///     });
	///     Ok(())
	/// });
	/// ```
	pub fn register_subscription<F>(
		&mut self,
		subscribe_method_name: &'static str,
		notif_method_name: &'static str,
		unsubscribe_method_name: &'static str,
		callback: F,
	) -> Result<(), Error>
	where
		Context: Send + Sync + 'static,
		F: Fn(Params, SubscriptionSink, Arc<Context>) -> Result<(), Error> + Send + Sync + 'static,
	{
		if subscribe_method_name == unsubscribe_method_name {
			return Err(Error::SubscriptionNameConflict(subscribe_method_name.into()));
		}

		self.methods.verify_method_name(subscribe_method_name)?;
		self.methods.verify_method_name(unsubscribe_method_name)?;

		let ctx = self.ctx.clone();
		let subscribers = Subscribers::default();

		// Subscribe
		{
			let subscribers = subscribers.clone();
			self.methods.mut_callbacks().insert(
				subscribe_method_name,
				MethodCallback::new_subscription(Arc::new(move |id, params, method_sink, conn| {
					let (conn_tx, conn_rx) = oneshot::channel::<()>();

					let sub_id = {
						let sub_id: RpcSubscriptionId = conn.id_provider.next_id().into_owned();
						let uniq_sub = SubscriptionKey { conn_id: conn.conn_id, sub_id: sub_id.clone() };

						subscribers.lock().insert(uniq_sub, (method_sink.clone(), conn_rx));

						sub_id
					};

					method_sink.send_response(id.clone(), &sub_id);

					let sink = SubscriptionSink {
						inner: method_sink.clone(),
						close_notify: Some(conn.close_notify),
						method: notif_method_name,
						subscribers: subscribers.clone(),
						uniq_sub: SubscriptionKey { conn_id: conn.conn_id, sub_id },
						is_connected: Some(conn_tx),
					};
					if let Err(err) = callback(params, sink, ctx.clone()) {
						tracing::error!(
							"subscribe call '{}' failed: {:?}, request id={:?}",
							subscribe_method_name,
							err,
							id
						);
						method_sink.send_error(id, ErrorCode::ServerError(CALL_EXECUTION_FAILED_CODE).into())
					} else {
						true
					}
				})),
			);
		}

		// Unsubscribe
		{
			self.methods.mut_callbacks().insert(
				unsubscribe_method_name,
				MethodCallback::new_subscription(Arc::new(move |id, params, sink, conn| {
					let sub_id = match params.one::<RpcSubscriptionId>() {
						Ok(sub_id) => sub_id,
						Err(_) => {
							tracing::error!(
								"unsubscribe call '{}' failed: couldn't parse subscription id={:?} request id={:?}",
								unsubscribe_method_name,
								params,
								id
							);
							return sink.send_response(id, false);
						}
					};
					let sub_id = sub_id.into_owned();

					let result = subscribers
						.lock()
						.remove(&SubscriptionKey { conn_id: conn.conn_id, sub_id: sub_id.clone() })
						.is_some();

					sink.send_response(id, result)
				})),
			);
		}

		Ok(())
	}

	/// Register an alias for an existing_method. Alias uniqueness is enforced.
	pub fn register_alias(&mut self, alias: &'static str, existing_method: &'static str) -> Result<(), Error> {
		self.methods.verify_method_name(alias)?;

		let callback = match self.methods.callbacks.get(existing_method) {
			Some(callback) => callback.clone(),
			None => return Err(Error::MethodNotFound(existing_method.into())),
		};

		self.methods.mut_callbacks().insert(alias, callback);

		Ok(())
	}
}

/// Represents a single subscription.
#[derive(Debug)]
pub struct SubscriptionSink {
	/// Sink.
	inner: MethodSink,
	/// Get notified when subscribers leave so we can exit
	close_notify: Option<Arc<Notify>>,
	/// MethodCallback.
	method: &'static str,
	/// Unique subscription.
	uniq_sub: SubscriptionKey,
	/// Shared Mutex of subscriptions for this method.
	subscribers: Subscribers,
	/// A type to track whether the subscription is active (the subscriber is connected).
	///
	/// None - implies that the subscription as been closed.
	is_connected: Option<oneshot::Sender<()>>,
}

impl SubscriptionSink {
	/// Send a message back to subscribers.
	pub fn send<T: Serialize>(&mut self, result: &T) -> Result<(), Error> {
		if self.is_closed() {
			return Err(Error::SubscriptionClosed(SubscriptionClosedReason::ConnectionReset.into()));
		}
		let msg = self.build_message(result)?;
		self.inner_send(msg).map_err(Into::into)
	}

	/// Consumes the `SubscriptionSink` and reads data from the `stream` and sends back data on the subscription
	/// when items gets produced by the stream.
	///
	/// Returns `Ok(())` if the stream or connection was terminated.
	/// Returns `Err(_)` if one of the items couldn't be serialized.
	///
	/// # Examples
	///
	/// ```no_run
	///
	/// use jsonrpsee_core::server::rpc_module::RpcModule;
	///
	/// let mut m = RpcModule::new(());
	/// m.register_subscription("sub", "_", "unsub", |params, mut sink, _| {
	///     let stream = futures_util::stream::iter(vec![1_u32, 2, 3]);
	///     tokio::spawn(sink.pipe_from_stream(stream));
	///     Ok(())
	/// });
	/// ```
	pub async fn pipe_from_stream<S, T>(mut self, mut stream: S) -> Result<(), Error>
	where
		S: Stream<Item = T> + Unpin,
		T: Serialize,
	{
		if let Some(close_notify) = self.close_notify.clone() {
			let mut stream_item = stream.next();
			let closed_fut = close_notify.notified();
			pin_mut!(closed_fut);
			loop {
				match futures_util::future::select(stream_item, closed_fut).await {
					// The app sent us a value to send back to the subscribers
					Either::Left((Some(result), next_closed_fut)) => {
						match self.send(&result) {
							Ok(_) => (),
							Err(Error::SubscriptionClosed(close_reason)) => {
								self.close(&close_reason);
								break Ok(());
							}
							Err(err) => {
								break Err(err);
							}
						};
						stream_item = stream.next();
						closed_fut = next_closed_fut;
					}
					// Stream terminated.
					Either::Left((None, _)) => break Ok(()),
					// The subscriber went away without telling us.
					Either::Right(((), _)) => {
						self.close(&SubscriptionClosed::new(SubscriptionClosedReason::ConnectionReset));
						break Ok(());
					}
				}
			}
		} else {
			// The sink is closed.
			Ok(())
		}
	}

	/// Returns whether this channel is closed without needing a context.
	pub fn is_closed(&self) -> bool {
		self.inner.is_closed() || self.close_notify.is_none()
	}

	fn build_message<T: Serialize>(&self, result: &T) -> Result<String, Error> {
		serde_json::to_string(&SubscriptionResponse::new(
			self.method.into(),
			SubscriptionPayload { subscription: self.uniq_sub.sub_id.clone(), result },
		))
		.map_err(Into::into)
	}

	fn inner_send(&mut self, msg: String) -> Result<(), Error> {
		let res = match self.is_connected.as_ref() {
			Some(conn) if !conn.is_canceled() => {
				// unbounded send only fails if the receiver has been dropped.
				self.inner.send_raw(msg).map_err(|_| Some(SubscriptionClosedReason::ConnectionReset))
			}
			Some(_) => Err(Some(SubscriptionClosedReason::Unsubscribed)),
			// NOTE(niklasad1): this should be unreachable, after the first error is detected the subscription is closed.
			None => Err(None),
		};

		// The subscription was already closed by the client
		// Close down the subscription but don't send a message to the client.
		if res.is_err() {
			self.inner_close(None);
		}

		res.map_err(|e| {
			let err = e.unwrap_or_else(|| SubscriptionClosedReason::Server("Close reason unknown".to_string()));
			Error::SubscriptionClosed(err.into())
		})
	}

	/// Close the subscription sink with a customized error message.
	pub fn close_with_custom_message(&mut self, msg: &str) {
		let close_reason = SubscriptionClosedReason::Server(msg.to_string()).into();
		self.inner_close(Some(&close_reason));
	}

	/// Close the subscription sink with the provided [`SubscriptionClosed`].
	pub fn close(&mut self, close_reason: &SubscriptionClosed) {
		self.inner_close(Some(close_reason));
	}

	fn inner_close(&mut self, close_reason: Option<&SubscriptionClosed>) {
		self.is_connected.take();
		if let Some((sink, _)) = self.subscribers.lock().remove(&self.uniq_sub) {
			tracing::debug!("Closing subscription: {:?} reason: {:?}", self.uniq_sub.sub_id, close_reason);
			if let Some(close_reason) = close_reason {
				let msg = self.build_message(close_reason).expect("valid json infallible; qed");
				let _ = sink.send_raw(msg);
			}
		}
	}
}

impl Drop for SubscriptionSink {
	fn drop(&mut self) {
		let err = SubscriptionClosedReason::Server("No close reason provided".into()).into();
		self.inner_close(Some(&err));
	}
}

/// Wrapper struct that maintains a subscription "mainly" for testing.
#[derive(Debug)]
pub struct Subscription {
	close_notify: Option<Arc<Notify>>,
	rx: mpsc::UnboundedReceiver<String>,
	sub_id: RpcSubscriptionId<'static>,
}

impl Subscription {
	/// Close the subscription channel.
	pub fn close(&mut self) {
		tracing::trace!("[Subscription::close] Notifying");
		if let Some(n) = self.close_notify.take() {
			n.notify_one()
		}
	}
	/// Get the subscription ID
	pub fn subscription_id(&self) -> &RpcSubscriptionId {
		&self.sub_id
	}

	/// Returns `Some((val, sub_id))` for the next element of type T from the underlying stream,
	/// otherwise `None` if the subscription was closed.
	///
	/// # Panics
	///
	/// If the decoding the value as `T` fails.
	pub async fn next<T: DeserializeOwned>(&mut self) -> Option<Result<(T, RpcSubscriptionId<'static>), Error>> {
		if self.close_notify.is_none() {
			tracing::debug!("[Subscription::next] Closed.");
			return Some(Err(Error::SubscriptionClosed(SubscriptionClosedReason::ConnectionReset.into())));
		}
		let raw = self.rx.next().await?;
		let res = match serde_json::from_str::<SubscriptionResponse<T>>(&raw) {
			Ok(r) => Ok((r.params.result, r.params.subscription.into_owned())),
			Err(_) => match serde_json::from_str::<SubscriptionResponse<SubscriptionClosed>>(&raw) {
				Ok(e) => Err(Error::SubscriptionClosed(e.params.result)),
				Err(e) => Err(e.into()),
			},
		};
		Some(res)
	}
}

impl Drop for Subscription {
	fn drop(&mut self) {
		self.close();
	}
}
