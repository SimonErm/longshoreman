use super::{BodyType, HttpClient};
use crate::{Compat, Error, Result};
use futures_util::{
    future::TryFutureExt,
    io::{AsyncRead, AsyncWrite},
    stream::{Stream, StreamExt, TryStreamExt},
};
use hyper::{
    body::{to_bytes, Bytes},
    header::IntoHeaderName,
    Body, Method, Request, StatusCode,
};
use serde::{Deserialize, Serialize};
use std::{convert::TryFrom, io};
use tokio_util::codec::{Decoder, FramedRead};

pub(crate) struct RequestBuilder<'a> {
    http_client: &'a HttpClient,
    uri_base: String,
    query: Option<String>,
    body: Option<BodyType>,
    builder: http::request::Builder,
}

impl<'a> RequestBuilder<'a> {
    pub fn new(http_client: &'a HttpClient, endpoint: impl AsRef<str>) -> Self {
        let uri_base = http_client.uri(endpoint.as_ref());
        let query = None;
        let body = None;
        let mut builder = hyper::Request::builder();
        builder = builder.method(Method::GET);

        debug_assert!(builder.method_ref().is_some());

        Self {
            http_client,
            uri_base,
            query,
            body,
            builder,
        }
    }

    // Builder methods

    pub fn method(mut self, method: Method) -> Self {
        self.builder = self.builder.method(method);
        self
    }

    pub fn header(mut self, key: impl IntoHeaderName, value: &str) -> Self {
        let value = http::header::HeaderValue::from_str(value).unwrap();
        self.builder.headers_mut().unwrap().append(key, value);
        self
    }

    pub fn query<T: Serialize>(mut self, query: T) -> Self {
        let query_string = serde_urlencoded::ser::to_string(query).unwrap();

        self.query = Some(query_string);
        self
    }

    pub fn json_body<T: Serialize>(mut self, body: T) -> Self {
        let data = serde_json::to_vec(&body).unwrap();
        self.body = Some(BodyType::json(data));
        self
    }

    pub fn tar_body(mut self, data: Vec<u8>) -> Self {
        self.body = Some(BodyType::tar(data));
        self
    }

    // Finalisers

    fn into_request(self) -> Result<(Request<Body>, &'a HttpClient)> {
        let uri = if let Some(query_string) = &self.query {
            format!("{}?{}", self.uri_base, query_string)
        } else {
            self.uri_base
        };

        println!("uri: {}", &uri);

        let mut builder = self.builder;
        debug_assert!(builder.method_ref().is_some());
        builder = builder.uri(uri);
        debug_assert!(builder.method_ref().is_some());

        let request = match self.body {
            Some(body_type) => {
                let mime = http::HeaderValue::try_from(body_type.mime())?;
                let data = body_type.into_data();
                let body = Body::from(data);

                builder.headers_mut().unwrap().append("content-type", mime);
                builder.body(body)?
            }
            None => builder.body(Body::empty())?,
        };

        Ok((request, self.http_client))
    }

    pub async fn into_response(self) -> Result<hyper::Response<Body>> {
        let (request, client) = self.into_request()?;
        Ok(client.send_request(request).await?)
    }

    async fn into_body(self) -> Result<Body> {
        let response = self.into_response().await?;
        let status = response.status();

        match status {
            // Success case: pass on the response
            StatusCode::OK
            | StatusCode::CREATED
            | StatusCode::SWITCHING_PROTOCOLS
            | StatusCode::NO_CONTENT => Ok(response.into_body()),
            // Error case: parse the text
            _ => {
                let bytes = to_bytes(response.into_body()).await?;

                Err(Error::Fault {
                    code: status,
                    message: get_error_message(&bytes).unwrap_or_else(|_| {
                        status
                            .canonical_reason()
                            .unwrap_or_else(|| "unknown error code")
                            .to_owned()
                    }),
                })
            }
        }
    }

    pub async fn upgrade(mut self) -> Result<impl AsyncRead + AsyncWrite> {
        self = self.header(hyper::header::CONNECTION, "Upgrade");
        self = self.header(hyper::header::UPGRADE, "tcp");

        let hyper_response = self.into_response().await?;

        match hyper_response.status() {
            StatusCode::SWITCHING_PROTOCOLS => {
                Ok(Compat::new(hyper_response.into_body().on_upgrade().await?))
            }
            _ => Err(Error::ConnectionNotUpgraded),
        }
    }

    async fn into_bytes(self) -> Result<Bytes> {
        let body = self.into_body().await?;
        Ok(to_bytes(body).await?)
    }

    pub async fn into_string(self) -> Result<String> {
        let bytes = self.into_bytes().await?;
        Ok(String::from_utf8(bytes.to_vec())?)
    }

    pub async fn into_json<T>(self) -> Result<T>
    where
        for<'de> T: Deserialize<'de>,
    {
        let bytes = self.into_bytes().await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn into_stream(self) -> impl Stream<Item = Result<Bytes>> + 'a {
        async fn unfold(mut body: Body) -> Option<(Result<Bytes>, Body)> {
            let bytes_result = body.next().await?.map_err(Error::from);

            Some((bytes_result, body))
        }

        async move {
            let body = self.into_body().await?;
            Ok(futures_util::stream::unfold(body, unfold))
        }
        .try_flatten_stream()
    }

    pub fn into_stream_json<T>(self) -> impl Stream<Item = Result<T>> + 'a
    where
        for<'de> T: Deserialize<'de>,
    {
        let byte_stream = self.into_stream();

        byte_stream.and_then(|bytes| async move { Ok(serde_json::from_slice(&bytes)?) })
    }

    pub fn decode<T, C, I, E>(self, codec: C) -> impl Stream<Item = Result<T>> + 'a
    where
        for<'de> T: Deserialize<'de>,
        C: Decoder<Item = I, Error = E> + 'a,
        I: AsRef<[u8]> + 'a,
        Error: From<E>,
        E: From<std::io::Error> + 'a,
    {
        let stream = Box::pin(
            self.into_stream()
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e)),
        );
        let reader = Compat::new(stream.into_async_read());
        FramedRead::new(reader, codec)
            .map_err(Error::from)
            .and_then(|bytes| async move { Ok(serde_json::from_slice(bytes.as_ref())?) })
    }
}

fn get_error_message(bytes: impl AsRef<[u8]>) -> Result<String> {
    #[derive(Serialize, Deserialize)]
    struct ErrorResponse {
        message: String,
    }

    let error_response: ErrorResponse = serde_json::from_slice(bytes.as_ref())?;
    Ok(error_response.message)
}