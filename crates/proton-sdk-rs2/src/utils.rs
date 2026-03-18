use serde::de::DeserializeOwned;

/// If a type has a representation as a protobuf, it will convert to that type.
pub trait AsProtobuf<To> {
    /// Converts `self` to a protobuf.
    fn as_protobuf(&self) -> To;
}

/// Converts from a protobuf type (as denoted with [`From`]) into the rust representation.
pub trait FromProtobuf<From> {
    /// Converts from a protobuf value into `Self`/the rust-native type.
    fn from_protobuf(value: &From) -> Self;
}

pub mod response_result_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use crate::protobuf::response::Result as ResponseResult;
    use crate::protobuf::Error;

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "PascalCase")]
    enum ResultHelper {
        Value(super::any_serde_helper::AnyHelper),
        Error(Error),
    }

    pub fn serialize<S: Serializer>(t: &Option<ResponseResult>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(ResponseResult::Value(any)) => {
                s.serialize_some(&ResultHelper::Value(super::any_serde_helper::AnyHelper {
                    type_url: any.type_url.clone(),
                    value: any.value.clone(),
                }))
            }
            Some(ResponseResult::Error(err)) => {
                s.serialize_some(&ResultHelper::Error(err.clone()))
            }
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<ResponseResult>, D::Error> {
        let h = Option::<ResultHelper>::deserialize(d)?;
        Ok(h.map(|h| match h {
            ResultHelper::Value(any) => ResponseResult::Value(super::Any {
                type_url: any.type_url,
                value: any.value,
            }),
            ResultHelper::Error(err) => ResponseResult::Error(err),
        }))
    }
}

pub mod any_serde_helper {
    use serde::{Deserialize, Serialize};
    #[derive(Serialize, Deserialize)]
    pub struct AnyHelper {
        pub type_url: String,
        pub value: Vec<u8>,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct Timestamp {
    pub seconds: i64,
    pub nanos: i32,
}

impl prost::Message for Timestamp {
    fn encode_raw<B: prost::bytes::BufMut>(&self, _buf: &mut B) {}
    fn merge_field<B: prost::bytes::Buf>(&mut self, _tag: u32, _wire_type: prost::encoding::WireType, _buf: &mut B, _ctx: prost::encoding::DecodeContext) -> Result<(), prost::DecodeError> { Ok(()) }
    fn encoded_len(&self) -> usize { 0 }
    fn clear(&mut self) {}
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct Any {
    pub type_url: String,
    pub value: Vec<u8>,
}

impl prost::Message for Any {
    fn encode_raw<B: prost::bytes::BufMut>(&self, _buf: &mut B) {}
    fn merge_field<B: prost::bytes::Buf>(&mut self, _tag: u32, _wire_type: prost::encoding::WireType, _buf: &mut B, _ctx: prost::encoding::DecodeContext) -> Result<(), prost::DecodeError> { Ok(()) }
    fn encoded_len(&self) -> usize { 0 }
    fn clear(&mut self) {}
}

pub(crate) async fn decode_json<T: DeserializeOwned>(response: reqwest::Response) -> anyhow::Result<T> {
    let body = response.bytes().await?;
    Ok(serde_json::from_slice::<T>(&body)?)
}

#[macro_export]
macro_rules! define_id {
    ($t:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $t(String);

        impl $t {
            pub fn new(value: String) -> Self { Self(value) }
            pub fn raw(&self) -> &str { &self.0 }
        }

        impl Default for $t {
            fn default() -> Self { Self(String::new()) }
        }
    };
}