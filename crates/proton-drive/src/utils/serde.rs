use chrono::{DateTime, TimeZone, Utc};
use proton_rpgp::pgp::packet::{Packet, PacketParser, Signature};
use serde::{Deserialize, Deserializer};

pub mod timestamp {
    pub use proton_sdk_rs2::utils::Timestamp;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(t: &Timestamp, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Helper {
            seconds: i64,
            nanos: i32,
        }
        Helper {
            seconds: t.seconds,
            nanos: t.nanos,
        }
        .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Timestamp, D::Error> {
        #[derive(Deserialize)]
        struct Helper {
            seconds: i64,
            nanos: i32,
        }
        let h = Helper::deserialize(d)?;
        Ok(Timestamp {
            seconds: h.seconds,
            nanos: h.nanos,
        })
    }
}

pub mod timestamp_opt {
    pub use proton_sdk_rs2::utils::Timestamp;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(t: &Option<Timestamp>, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Helper {
            seconds: i64,
            nanos: i32,
        }
        t.as_ref()
            .map(|t| Helper {
                seconds: t.seconds,
                nanos: t.nanos,
            })
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Timestamp>, D::Error> {
        #[derive(Deserialize)]
        struct Helper {
            seconds: i64,
            nanos: i32,
        }
        let h = Option::<Helper>::deserialize(d)?;
        Ok(h.map(|h| Timestamp {
            seconds: h.seconds,
            nanos: h.nanos,
        }))
    }
}

pub fn deserialize_signature<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<Signature>, D::Error> {
    let Some(s) = Option::<String>::deserialize(d)? else {
        return Ok(None);
    };
    let bytes = hex::decode(&s).map_err(serde::de::Error::custom)?;
    let mut parser = PacketParser::new(&bytes[..]);
    let packet = parser
        .next()
        .ok_or_else(|| serde::de::Error::custom("Empty signature"))?
        .map_err(serde::de::Error::custom)?;
    match packet {
        Packet::Signature(sig) => Ok(Some(sig)),
        _ => Err(serde::de::Error::custom("Expected signature packet")),
    }
}

pub fn deserialize_time<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
    let secs = i64::deserialize(d)?;
    Utc.timestamp_opt(secs, 0)
        .single()
        .ok_or_else(|| serde::de::Error::custom("invalid epoch seconds"))
}

pub(crate) mod base64_bytes {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(&s).map_err(serde::de::Error::custom)
    }
}

pub mod base64_bytes_opt {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserializer, Serializer, Deserialize};

    pub fn serialize<S: Serializer>(bytes: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(b) => s.serialize_some(&STANDARD.encode(b)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let s = Option::<String>::deserialize(d)?;
        match s {
            None => Ok(None),
            Some(s) => STANDARD.decode(&s).map(Some).map_err(serde::de::Error::custom),
        }
    }
}


pub(crate) mod forgiving_hex_bytes {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;

        if let Ok(bytes) = hex::decode(&s) {
            return Ok(bytes);
        }

        STANDARD.decode(&s).map_err(|_| {
            serde::de::Error::custom(format!(
                "Hash field: could not decode '{}' as hex or base64",
                s
            ))
        })
    }
}

pub mod epoch_seconds_opt {
    use chrono::{DateTime, TimeZone, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(dt: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match dt {
            Some(dt) => s.serialize_some(&dt.timestamp()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        let ts = Option::<i64>::deserialize(d)?;
        Ok(ts.and_then(|t| Utc.timestamp_opt(t, 0).single()))
    }
}

pub mod forgiving_hex_bytes_opt {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(b) => s.serialize_some(&hex::encode(b)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        let s = Option::<String>::deserialize(d)?;
        match s {
            None => Ok(None),
            Some(s) => {
                if let Ok(bytes) = hex::decode(&s) {
                    return Ok(Some(bytes));
                }
                STANDARD.decode(&s).map(Some).map_err(|_| {
                    serde::de::Error::custom(format!(
                        "SHA1 field: could not decode '{}' as hex or base64",
                        s
                    ))
                })
            }
        }
    }
}

pub mod epoch_seconds {
    use chrono::{DateTime, TimeZone, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(dt: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(dt.timestamp())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let ts = i64::deserialize(d)?;
        Utc.timestamp_opt(ts, 0)
            .single()
            .ok_or_else(|| serde::de::Error::custom("invalid epoch timestamp"))
    }
}
