use crate as tg;
use bytes::Bytes;
use futures::{future, Stream, StreamExt as _, TryStreamExt as _};
use http_body_util::BodyStream;
use serde_with::serde_as;
use tangram_http::{incoming::ResponseExt as _, Outgoing};
use tokio_util::io::StreamReader;

#[serde_as]
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Arg {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub length: Option<i64>,

	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[serde_as(as = "Option<crate::util::serde::SeekFromString>")]
	pub position: Option<std::io::SeekFrom>,

	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub size: Option<u64>,

	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[serde_as(as = "Option<serde_with::DurationSeconds>")]
	pub timeout: Option<std::time::Duration>,
}

#[serde_as]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Chunk {
	pub position: u64,
	#[serde_as(as = "crate::util::serde::BytesBase64")]
	pub bytes: Bytes,
}

impl tg::Build {
	pub async fn log<H>(
		&self,
		handle: &H,
		arg: tg::build::log::Arg,
	) -> tg::Result<impl Stream<Item = tg::Result<tg::build::log::Chunk>> + Send + 'static>
	where
		H: tg::Handle,
	{
		self.try_get_log(handle, arg)
			.await?
			.ok_or_else(|| tg::error!("failed to get the build"))
	}

	pub async fn try_get_log<H>(
		&self,
		handle: &H,
		arg: tg::build::log::Arg,
	) -> tg::Result<Option<impl Stream<Item = tg::Result<tg::build::log::Chunk>> + Send + 'static>>
	where
		H: tg::Handle,
	{
		handle
			.try_get_build_log(self.id(), arg)
			.await
			.map(|option| option.map(futures::StreamExt::boxed))
	}

	pub async fn add_log<H>(&self, handle: &H, log: Bytes) -> tg::Result<()>
	where
		H: tg::Handle,
	{
		let id = self.id();
		handle.add_build_log(id, log).await?;
		Ok(())
	}
}

impl tg::Client {
	pub async fn try_get_build_log(
		&self,
		id: &tg::build::Id,
		arg: tg::build::log::Arg,
	) -> tg::Result<Option<impl Stream<Item = tg::Result<tg::build::log::Chunk>> + Send + 'static>>
	{
		let method = http::Method::GET;
		let query = serde_urlencoded::to_string(&arg).unwrap();
		let uri = format!("/builds/{id}/log?{query}");
		let body = Outgoing::empty();
		let request = http::request::Builder::default()
			.method(method)
			.uri(uri)
			.header(http::header::ACCEPT, mime::TEXT_EVENT_STREAM.to_string())
			.body(body)
			.unwrap();
		let response = self
			.send(request)
			.await
			.map_err(|source| tg::error!(!source, "failed to send the request"))?;
		if response.status() == http::StatusCode::NOT_FOUND {
			return Ok(None);
		}
		if !response.status().is_success() {
			let error = response.json().await?;
			return Err(error);
		}
		let reader = StreamReader::new(
			BodyStream::new(response.into_body())
				.try_filter_map(|frame| future::ok(frame.into_data().ok()))
				.map_err(std::io::Error::other),
		);
		let output = tangram_http::sse::Decoder::new(reader).map(|result| {
			let event = result.map_err(|source| tg::error!(!source, "failed to read an event"))?;
			let chunk = serde_json::from_str(&event.data)
				.map_err(|source| tg::error!(!source, "failed to deserialize the event data"))?;
			Ok::<_, tg::Error>(chunk)
		});
		Ok(Some(output))
	}

	pub async fn add_build_log(&self, id: &tg::build::Id, bytes: Bytes) -> tg::Result<()> {
		let method = http::Method::POST;
		let uri = format!("/builds/{id}/log");
		let request = http::request::Builder::default()
			.method(method)
			.uri(uri)
			.header(
				http::header::CONTENT_TYPE,
				mime::APPLICATION_OCTET_STREAM.to_string(),
			)
			.body(Outgoing::bytes(bytes))
			.unwrap();
		let response = self.send(request).await?;
		if !response.status().is_success() {
			let error = response.json().await?;
			return Err(error);
		}
		Ok(())
	}
}
