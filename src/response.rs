use crate::io::Text;
use crate::{Metrics, Error};
use crate::task::Join;
use futures_io::AsyncRead;
use futures_util::AsyncReadExt;
use http::{Response, Uri};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

type TextFuture<'a> = futures_util::future::LocalBoxFuture<'a, Result<String, Error>>;

/// Provides extension methods for working with HTTP responses.
pub trait ResponseExt<T> {
    /// Get the effective URI of this response. This value differs from the
    /// original URI provided when making the request if at least one redirect
    /// was followed.
    ///
    /// This information is only available if populated by the HTTP client that
    /// produced the response.
    fn effective_uri(&self) -> Option<&Uri>;

    /// If request metrics are enabled for this particular transfer, return a
    /// metrics object containing a live view of currently available data.
    ///
    /// By default metrics are disabled and `None` will be returned. To enable
    /// metrics for a single request you can use
    /// [`RequestExt::metrics`](crate::RequestBuilderExt::metrics), or to enable
    /// it client-wide, you can use
    /// [`HttpClientBuilder::metrics`](crate::HttpClientBuilder::metrics).
    fn metrics(&self) -> Option<&Metrics>;

    /// Copy the response body into a writer.
    ///
    /// Returns the number of bytes that were written.
    fn copy_to(&mut self, writer: impl Write) -> io::Result<u64>
    where
        T: Read;

    /// Write the response body to a file.
    ///
    /// This method makes it convenient to download a file using a GET request
    /// and write it to a file synchronously in a single chain of calls.
    ///
    /// Returns the number of bytes that were written.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use isahc::prelude::*;
    ///
    /// isahc::get("https://httpbin.org/image/jpeg")?
    ///     .copy_to_file("myimage.jpg")?;
    /// # Ok::<(), isahc::Error>(())
    /// ```
    fn copy_to_file(&mut self, path: impl AsRef<Path>) -> io::Result<u64>
    where
        T: Read,
    {
        File::create(path).and_then(|f| self.copy_to(f))
    }

    /// Get the response body as a string.
    ///
    /// This method consumes the entire response body stream and can only be
    /// called once, unless you can rewind this response body.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use isahc::prelude::*;
    ///
    /// let text = isahc::get("https://example.org")?.text()?;
    /// println!("{}", text);
    /// # Ok::<(), isahc::Error>(())
    /// ```
    fn text(&mut self) -> Result<String, Error>
    where
        T: Read;

    /// Get the response body as a string asynchronously.
    ///
    /// This method consumes the entire response body stream and can only be
    /// called once, unless you can rewind this response body.
    fn text_async(&mut self) -> TextFuture<'_>
    where
        T: AsyncRead + Unpin;

    /// Deserialize the response body as JSON into a given type.
    ///
    /// This method requires the `json` feature to be enabled.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use isahc::prelude::*;
    /// use serde_json::Value;
    ///
    /// let json: Value = isahc::get("https://httpbin.org/json")?.json()?;
    /// println!("author: {}", json["slideshow"]["author"]);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[cfg(feature = "json")]
    fn json<D>(&mut self) -> Result<D, serde_json::Error>
    where
        D: serde::de::DeserializeOwned,
        T: Read;
}

impl<T> ResponseExt<T> for Response<T> {
    fn effective_uri(&self) -> Option<&Uri> {
        self.extensions().get::<EffectiveUri>().map(|v| &v.0)
    }

    fn metrics(&self) -> Option<&Metrics> {
        self.extensions().get()
    }

    fn copy_to(&mut self, mut writer: impl Write) -> io::Result<u64>
    where
        T: Read,
    {
        io::copy(self.body_mut(), &mut writer)
    }

    #[cfg(feature = "encoding")]
    fn text(&mut self) -> Result<String, Error>
    where
        T: Read,
    {
        let encoding = get_encoding(self).unwrap();
        let mut decoder = encoding.new_decoder();
        let mut string = String::new();

        let mut buf = [0; 8192];

        loop {
            let len = match self.body_mut().read(&mut buf) {
                Ok(0) => break,
                Ok(len) => len,
                Err(e) => if e.kind() == io::ErrorKind::Interrupted {
                    continue
                } else {
                    return Err(e.into())
                },
            };

            match decoder.decode_to_string(&buf[..len], &mut string, false) {
                (encoding_rs::CoderResult::InputEmpty, _, _) => {

                }
                _ => {}
            }
        }

        Ok(string)
    }

    #[cfg(not(feature = "encoding"))]
    fn text(&mut self) -> Result<String, Error>
    where
        T: Read,
    {
        let mut string = String::new();
        self.body_mut().read_to_string(&mut string)?;

        Ok(string)
    }

    fn text_async(&mut self) -> TextFuture<'_>
    where
        T: AsyncRead + Unpin,
    {
        Box::pin(async move {
            let encoding = get_encoding(self).unwrap();
            let mut decoder = encoding.new_decoder();
            let mut string = String::new();

            let mut buf = [0; 8192];

            loop {
                let len = match self.body_mut().read(&mut buf).await {
                    Ok(0) => break,
                    Ok(len) => len,
                    Err(e) => if e.kind() == io::ErrorKind::Interrupted {
                        continue
                    } else {
                        return Err(e.into())
                    },
                };

                match decoder.decode_to_string(&buf[..len], &mut string, false) {
                    (encoding_rs::CoderResult::InputEmpty, _, _) => {

                    }
                    _ => {}
                }
            }

            Ok(string)
        })
    }

    #[cfg(feature = "json")]
    fn json<D>(&mut self) -> Result<D, serde_json::Error>
    where
        D: serde::de::DeserializeOwned,
        T: Read,
    {
        serde_json::from_reader(self.body_mut())
    }
}

pub(crate) struct EffectiveUri(pub(crate) Uri);

fn get_encoding<T>(response: &http::Response<T>) -> Option<encoding_rs::Encoding> {
    let content_type = response.headers().get(http::header::CONTENT_TYPE)?;

    let content_type = match content_type.to_str() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("could not parse Content-Type header: {}", e);
            return None;
        }
    };

    let content_type = match content_type.parse::<mime::Mime>() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("could not parse Content-Type header: {}", e);
            return None;
        }
    };

    .and_then(|mime| mime.get_param("charset"))
    .map(|charset| charset.as_str().as_bytes())
    .and_then(encoding_rs::Encoding::for_label)
    .unwrap_or(encoding_rs::UTF_8);
    None
}
