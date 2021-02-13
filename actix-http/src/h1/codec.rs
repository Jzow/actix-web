use std::{fmt, io};

use actix_codec::{Decoder, Encoder};
use bitflags::bitflags;
use bytes::BytesMut;
use http::{Method, Version};

use super::decoder::PayloadType;
use super::Message;
use super::{decoder, encoder};
use crate::body::BodySize;
use crate::config::ServiceConfig;
use crate::error::ParseError;
use crate::message::ConnectionType;
use crate::request::Request;
use crate::response::Response;

bitflags! {
    struct Flags: u8 {
        const HEAD              = 0b0000_0001;
        const KEEPALIVE_ENABLED = 0b0000_0010;
        const STREAM            = 0b0000_0100;
    }
}

/// HTTP/1 Codec
pub struct Codec {
    config: ServiceConfig,
    decoder: decoder::MessageDecoder<Request>,
    version: Version,
    ctype: ConnectionType,

    // encoder part
    flags: Flags,
    encoder: encoder::MessageEncoder<Response<()>>,
}

impl Default for Codec {
    fn default() -> Self {
        Codec::new(ServiceConfig::default())
    }
}

impl fmt::Debug for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "h1::Codec({:?})", self.flags)
    }
}

impl Codec {
    /// Create HTTP/1 codec.
    ///
    /// `keepalive_enabled` how response `connection` header get generated.
    pub fn new(config: ServiceConfig) -> Self {
        let flags = if config.keep_alive_enabled() {
            Flags::KEEPALIVE_ENABLED
        } else {
            Flags::empty()
        };

        Codec {
            config,
            flags,
            decoder: decoder::MessageDecoder::default(),
            version: Version::HTTP_11,
            ctype: ConnectionType::Close,
            encoder: encoder::MessageEncoder::default(),
        }
    }

    /// Check if request is upgrade.
    #[inline]
    pub fn upgrade(&self) -> bool {
        self.ctype == ConnectionType::Upgrade
    }

    /// Check if last response is keep-alive.
    #[inline]
    pub fn keepalive(&self) -> bool {
        self.ctype == ConnectionType::KeepAlive
    }

    /// Check if keep-alive enabled on server level.
    #[inline]
    pub fn keepalive_enabled(&self) -> bool {
        self.flags.contains(Flags::KEEPALIVE_ENABLED)
    }

    #[inline]
    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }
}

impl Decoder for Codec {
    type Item = (Request, PayloadType);
    type Error = ParseError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if let Some((req, payload)) = self.decoder.decode(src)? {
            let head = req.head();
            self.flags.set(Flags::HEAD, head.method == Method::HEAD);
            self.version = head.version;
            self.ctype = head.connection_type();
            if self.ctype == ConnectionType::KeepAlive
                && !self.flags.contains(Flags::KEEPALIVE_ENABLED)
            {
                self.ctype = ConnectionType::Close
            }

            if let PayloadType::Stream(_) = payload {
                self.flags.insert(Flags::STREAM);
            }
            Ok(Some((req, payload)))
        } else {
            Ok(None)
        }
    }
}

impl Encoder<Message<(Response<()>, BodySize)>> for Codec {
    type Error = io::Error;

    fn encode(
        &mut self,
        item: Message<(Response<()>, BodySize)>,
        dst: &mut BytesMut,
    ) -> Result<(), Self::Error> {
        match item {
            Message::Item((mut res, length)) => {
                // set response version
                res.head_mut().version = self.version;

                // connection status
                self.ctype = if let Some(ct) = res.head().ctype() {
                    if ct == ConnectionType::KeepAlive {
                        self.ctype
                    } else {
                        ct
                    }
                } else {
                    self.ctype
                };

                // encode message
                self.encoder.encode(
                    dst,
                    &mut res,
                    self.flags.contains(Flags::HEAD),
                    self.flags.contains(Flags::STREAM),
                    self.version,
                    length,
                    self.ctype,
                    &self.config,
                )?;
                // self.headers_size = (dst.len() - len) as u32;
            }
            Message::Chunk(Some(bytes)) => {
                self.encoder.encode_chunk(bytes.as_ref(), dst)?;
            }
            Message::Chunk(None) => {
                self.encoder.encode_eof(dst)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;
    use http::Method;

    use super::*;
    use crate::HttpMessage;

    #[actix_rt::test]
    async fn test_http_request_chunked_payload_and_next_message() {
        let mut codec = Codec::default();

        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             transfer-encoding: chunked\r\n\r\n",
        );
        let item = codec.decode(&mut buf).unwrap().unwrap();
        let req = item.message();

        assert_eq!(req.method(), Method::GET);
        assert!(req.chunked().unwrap());

        buf.extend(
            b"4\r\ndata\r\n4\r\nline\r\n0\r\n\r\n\
               POST /test2 HTTP/1.1\r\n\
               transfer-encoding: chunked\r\n\r\n"
                .iter(),
        );

        let msg = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"data");

        let msg = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"line");

        let msg = codec.decode(&mut buf).unwrap().unwrap();
        assert!(msg.eof());

        // decode next message
        let item = codec.decode(&mut buf).unwrap().unwrap();
        let req = item.message();
        assert_eq!(*req.method(), Method::POST);
        assert!(req.chunked().unwrap());
    }
}
