use crate::Server;
use bytes::Bytes;
use futures::{
	future, stream, stream_select, FutureExt as _, Stream, StreamExt as _, TryStreamExt as _,
};
use http_body_util::StreamBody;
use indoc::formatdoc;
use num::ToPrimitive as _;
use std::{pin::pin, sync::Arc};
use tangram_client as tg;
use tangram_database::{self as db, prelude::*};
use tangram_http::{
	incoming::RequestExt as _,
	outgoing::{ResponseBuilderExt, ResponseExt as _},
	Incoming, Outgoing,
};
use tangram_messenger::Messenger as _;
use tokio_stream::wrappers::IntervalStream;

impl Server {
	pub async fn try_get_build_children(
		&self,
		id: &tg::build::Id,
		arg: tg::build::children::Arg,
	) -> tg::Result<
		Option<impl Stream<Item = tg::Result<tg::build::children::Chunk>> + Send + 'static>,
	> {
		if let Some(children) = self.try_get_build_children_local(id, arg.clone()).await? {
			Ok(Some(children.left_stream()))
		} else if let Some(children) = self.try_get_build_children_remote(id, arg.clone()).await? {
			Ok(Some(children.right_stream()))
		} else {
			Ok(None)
		}
	}

	async fn try_get_build_children_local(
		&self,
		id: &tg::build::Id,
		arg: tg::build::children::Arg,
	) -> tg::Result<
		Option<impl Stream<Item = tg::Result<tg::build::children::Chunk>> + Send + 'static>,
	> {
		// Verify the build is local.
		if !self.get_build_exists_local(id).await? {
			return Ok(None);
		}

		// Create the event stream.
		let children = self
			.messenger
			.subscribe(format!("builds.{id}.children"), None)
			.await
			.map_err(|source| tg::error!(!source, "failed to subscribe"))?
			.map(|_| ())
			.boxed();
		let status = self
			.messenger
			.subscribe(format!("builds.{id}.status"), None)
			.await
			.map_err(|source| tg::error!(!source, "failed to subscribe"))?
			.map(|_| ())
			.boxed();
		let interval =
			IntervalStream::new(tokio::time::interval(std::time::Duration::from_secs(60)))
				.map(|_| ());
		let timeout = arg.timeout.map_or_else(
			|| future::pending().left_future(),
			|timeout| tokio::time::sleep(timeout).right_future(),
		);
		let events = stream::once(future::ready(()))
			.chain(stream_select!(children, status, interval).take_until(timeout))
			.boxed();

		// Get the position.
		let position = match arg.position {
			Some(std::io::SeekFrom::Start(seek)) => seek,
			Some(std::io::SeekFrom::End(seek) | std::io::SeekFrom::Current(seek)) => self
				.try_get_build_children_local_current_position(id)
				.await?
				.to_i64()
				.unwrap()
				.checked_add(seek)
				.ok_or_else(|| tg::error!("invalid offset"))?
				.to_u64()
				.ok_or_else(|| tg::error!("invalid offset"))?,
			None => {
				self.try_get_build_children_local_current_position(id)
					.await?
			},
		};

		// Get the length.
		let length = arg.length;

		// Get the size.
		let size = arg.size.unwrap_or(10);

		// Create the stream.
		struct State {
			position: u64,
			read: u64,
		}
		let state = State { position, read: 0 };
		let state = Arc::new(tokio::sync::Mutex::new(state));
		let stream = stream::try_unfold(
			(self.clone(), id.clone(), events, state, false),
			move |(server, id, mut events, state, mut end)| async move {
				if end {
					return Ok(None);
				}

				let read = state.lock().await.read;
				if length.is_some_and(|length| read >= length) {
					return Ok(None);
				}

				let Some(()) = events.next().await else {
					return Ok(None);
				};

				let arg = tg::build::status::Arg {
					timeout: Some(std::time::Duration::ZERO),
				};
				let status = server
					.try_get_build_status_local(&id, arg)
					.await?
					.ok_or_else(|| tg::error!("expected the build to exist"))?;
				let status = pin!(status)
					.try_next()
					.await?
					.ok_or_else(|| tg::error!("expected the status to exist"))?;
				if status == tg::build::Status::Finished {
					end = true;
				}

				// Create the stream.
				let stream = stream::try_unfold(
					(server.clone(), id.clone(), state.clone(), false),
					move |(server, id, state, end)| async move {
						if end {
							return Ok(None);
						}

						// Lock the state.
						let mut state_ = state.lock().await;

						// Determine the size.
						let size = match length {
							None => size,
							Some(length) => size.min(length - state_.read),
						};

						// Read the chunk.
						let chunk = server
							.try_get_build_children_local_inner(&id, state_.position, size)
							.await?;
						let read = chunk.items.len().to_u64().unwrap();

						// Update the state.
						state_.position += read;
						state_.read += read;

						drop(state_);

						// If the chunk is empty, then only return it if the build is finished and the position is at the end.
						if chunk.items.is_empty() {
							let end = server
								.try_get_build_children_local_end(&id, chunk.position)
								.await?;
							if end {
								return Ok::<_, tg::Error>(Some((
									chunk,
									(server, id, state, true),
								)));
							}
							return Ok(None);
						}

						Ok::<_, tg::Error>(Some((chunk, (server, id, state, false))))
					},
				);

				Ok::<_, tg::Error>(Some((stream, (server, id, events, state, end))))
			},
		)
		.try_flatten()
		.boxed();

		Ok(Some(stream))
	}

	async fn try_get_build_children_local_current_position(
		&self,
		id: &tg::build::Id,
	) -> tg::Result<u64> {
		// Get a database connection.
		let connection = self
			.database
			.connection()
			.await
			.map_err(|source| tg::error!(!source, "failed to get a database connection"))?;

		// Get the position.
		let p = connection.p();
		let statement = formatdoc!(
			"
				select count(*)
				from build_children
				where build = {p}1;
			"
		);
		let params = db::params![id];
		let position = connection
			.query_one_value_into(statement, params)
			.await
			.map_err(|source| tg::error!(!source, "failed to execute the statement"))?;

		// Drop the database connection.
		drop(connection);

		Ok(position)
	}

	async fn try_get_build_children_local_end(
		&self,
		id: &tg::build::Id,
		position: u64,
	) -> tg::Result<bool> {
		// Get a database connection.
		let connection = self
			.database
			.connection()
			.await
			.map_err(|source| tg::error!(!source, "failed to get a database connection"))?;

		// Get the end.
		let p = connection.p();
		let statement = formatdoc!(
			"
				select (
					select status = 'finished'
					from builds
					where id = {p}1
				) and (
					select {p}2 >= count(*)
					from build_children
					where build = {p}1
				);
			"
		);
		let params = db::params![id, position];
		let end = connection
			.query_one_value_into(statement, params)
			.await
			.map_err(|source| tg::error!(!source, "failed to execute the statement"))?;

		// Drop the database connection.
		drop(connection);

		Ok(end)
	}

	async fn try_get_build_children_local_inner(
		&self,
		id: &tg::build::Id,
		position: u64,
		length: u64,
	) -> tg::Result<tg::build::children::Chunk> {
		// Get a database connection.
		let connection = self
			.database
			.connection()
			.await
			.map_err(|source| tg::error!(!source, "failed to get a database connection"))?;

		// Get the children.
		let p = connection.p();
		let statement = formatdoc!(
			"
				select child
				from build_children
				where build = {p}1
				order by position
				limit {p}2
				offset {p}3
			"
		);
		let params = db::params![id, length, position,];
		let children = connection
			.query_all_value_into(statement, params)
			.await
			.map_err(|source| tg::error!(!source, "failed to execute the statement"))?;

		// Drop the database connection.
		drop(connection);

		// Create the chunk.
		let chunk = tg::build::children::Chunk {
			position,
			items: children,
		};

		Ok(chunk)
	}

	async fn try_get_build_children_remote(
		&self,
		id: &tg::build::Id,
		arg: tg::build::children::Arg,
	) -> tg::Result<
		Option<impl Stream<Item = tg::Result<tg::build::children::Chunk>> + Send + 'static>,
	> {
		let Some(remote) = self.remotes.first() else {
			return Ok(None);
		};
		let Some(stream) = remote.try_get_build_children(id, arg).await? else {
			return Ok(None);
		};
		Ok(Some(stream))
	}

	pub async fn add_build_child(
		&self,
		build_id: &tg::build::Id,
		child_id: &tg::build::Id,
	) -> tg::Result<()> {
		if self.try_add_build_child_local(build_id, child_id).await? {
			return Ok(());
		}
		if self.try_add_build_child_remote(build_id, child_id).await? {
			return Ok(());
		}
		Err(tg::error!("failed to get the build"))
	}

	async fn try_add_build_child_local(
		&self,
		build_id: &tg::build::Id,
		child_id: &tg::build::Id,
	) -> tg::Result<bool> {
		// Verify the build is local.
		if !self.get_build_exists_local(build_id).await? {
			return Ok(false);
		}

		// Get a database connection.
		let connection = self
			.database
			.connection()
			.await
			.map_err(|source| tg::error!(!source, "failed to get a database connection"))?;

		// Add the child to the database.
		let p = connection.p();
		let statement = formatdoc!(
			"
				insert into build_children (build, position, child)
				values ({p}1, (select coalesce(max(position) + 1, 0) from build_children where build = {p}1), {p}2)
				on conflict (build, child) do nothing;
			"
		);
		let params = db::params![build_id, child_id];
		connection
			.execute(statement, params)
			.await
			.map_err(|source| tg::error!(!source, "failed to execute the statement"))?;

		// Drop the database connection.
		drop(connection);

		// Publish the message.
		self.messenger
			.publish(format!("builds.{build_id}.children"), Bytes::new())
			.await
			.map_err(|source| tg::error!(!source, "failed to publish"))?;

		Ok(true)
	}

	async fn try_add_build_child_remote(
		&self,
		build_id: &tg::build::Id,
		child_id: &tg::build::Id,
	) -> tg::Result<bool> {
		let Some(remote) = self.remotes.first() else {
			return Ok(false);
		};
		tg::Build::with_id(child_id.clone())
			.push(self, remote)
			.await?;
		remote.add_build_child(build_id, child_id).await?;
		Ok(true)
	}
}

impl Server {
	pub(crate) async fn handle_get_build_children_request<H>(
		handle: &H,
		request: http::Request<Incoming>,
		id: &str,
	) -> tg::Result<http::Response<Outgoing>>
	where
		H: tg::Handle,
	{
		let id = id.parse()?;
		let arg = request
			.uri()
			.query()
			.map(serde_urlencoded::from_str)
			.transpose()
			.map_err(|source| tg::error!(!source, "failed to deserialize the query"))?
			.unwrap_or_default();

		// Get the accept header.
		let accept = request
			.headers()
			.get(http::header::ACCEPT)
			.map(|accept| {
				let accept = accept
					.to_str()
					.map_err(|source| tg::error!(!source, "invalid content type"))?;
				let accept = accept
					.parse::<mime::Mime>()
					.map_err(|source| tg::error!(!source, "invalid content type"))?;
				Ok::<_, tg::Error>(accept)
			})
			.transpose()?;

		// Get the stream.
		let Some(stream) = handle.try_get_build_children(&id, arg).await? else {
			return Ok(http::Response::not_found());
		};

		// Stop the stream when the server stops.
		let mut stop = request
			.extensions()
			.get::<tokio::sync::watch::Receiver<bool>>()
			.cloned()
			.unwrap();
		let stop = async move { stop.wait_for(|stop| *stop).map(|_| ()).await };
		let stream = stream.take_until(stop);

		// Create the body.
		let (content_type, body) = match accept
			.as_ref()
			.map(|accept| (accept.type_(), accept.subtype()))
		{
			None | Some((mime::STAR, mime::STAR) | (mime::APPLICATION, mime::JSON)) => {
				let content_type = mime::APPLICATION_JSON;
				let body = stream::once(async move {
					let children: Vec<tg::build::Id> = stream
						.map_ok(|chunk| stream::iter(chunk.items).map(Ok::<_, tg::Error>))
						.try_flatten()
						.try_collect()
						.await?;
					let json = serde_json::to_string(&children)
						.map_err(|source| tg::error!(!source, "failed to serialize the body"))?;
					Ok(hyper::body::Frame::data(json.into_bytes().into()))
				});
				let body = Outgoing::body(StreamBody::new(body));
				(content_type, body)
			},
			Some((mime::TEXT, mime::EVENT_STREAM)) => {
				let content_type = mime::TEXT_EVENT_STREAM;
				let body = stream
					.map_ok(|chunk| {
						let data = serde_json::to_string(&chunk).unwrap();
						let event = tangram_http::sse::Event::with_data(data);
						hyper::body::Frame::data(event.to_string().into())
					})
					.err_into();
				let body = Outgoing::body(StreamBody::new(body));
				(content_type, body)
			},
			_ => {
				return Err(tg::error!(?accept, "invalid accept header"));
			},
		};

		// Create the response.
		let response = http::Response::builder()
			.status(http::StatusCode::OK)
			.header(http::header::CONTENT_TYPE, content_type.to_string())
			.body(body)
			.unwrap();

		Ok(response)
	}

	pub(crate) async fn handle_add_build_child_request<H>(
		handle: &H,
		request: http::Request<Incoming>,
		id: &str,
	) -> tg::Result<http::Response<Outgoing>>
	where
		H: tg::Handle,
	{
		let build_id = id.parse()?;
		let child_id = request.json().await?;
		handle.add_build_child(&build_id, &child_id).await?;
		let response = http::Response::builder()
			.status(http::StatusCode::OK)
			.empty()
			.unwrap();
		Ok(response)
	}
}
