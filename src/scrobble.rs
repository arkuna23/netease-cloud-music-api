//! Desktop NCBL v3 playback uploads.
//!
//! Wire-format reference: api-enhanced 19c7dad6db7d660954c7a615a686ff0acb030f91,
//! util/ncbl.js (MIT). This implementation constructs records from actual playback
//! metadata; callers own timing, account isolation and delivery policy.

use crate::MusicApi;
use flate2::{write::GzEncoder, Compression};
use isahc::{config::RedirectPolicy, prelude::*, Request};
use openssl::{
    bn::{BigNum, BigNumContext},
    rand::rand_bytes,
    symm::{Cipher, Crypter, Mode},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fmt,
    io::Write,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const UPLOAD_URL: &str =
    "https://clientlog3.music.163.com/api/clientlog/encrypt/upload?multiupload=true";
const VERSION: &str = "3.1.35.205293";

/// A real playback transition within one local session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackEventKind {
    Start,
    End,
}

/// Known playback origin. Empty fields represent an unknown origin.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PlaybackSource {
    pub id: String,
    pub kind: String,
    pub name: String,
}

/// Song metadata and measured listening time. All durations use `Duration`.
#[derive(Debug, Clone)]
pub struct PlaybackEvent {
    pub session_id: u64,
    pub kind: PlaybackEventKind,
    pub song_id: u64,
    pub duration: Duration,
    pub played: Duration,
    pub source: PlaybackSource,
    /// Actual encoded bitrate, in bits per second; zero represents unknown.
    pub bitrate: u32,
    pub quality: String,
    pub timestamp: SystemTime,
}

/// Frozen authentication for a single account generation.
/// Credentials intentionally have a private representation.
#[derive(Clone)]
pub struct ScrobbleContext {
    pub account_id: u64,
    fields: BTreeMap<String, String>,
}

/// The service acknowledged this upload file. Official statistics are verified separately.
#[derive(Debug, Clone)]
pub struct ScrobbleReceipt {
    pub file_name: String,
}

/// Upload outcome, suitable for credential-free diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScrobbleError {
    Authentication,
    Encoding,
    Transport(isahc::error::ErrorKind),
    Http(u16),
    Business {
        code: i64,
        rate: Option<i64>,
    },
    /// The request may have arrived; its acknowledgement could not be established.
    Unknown,
}
impl fmt::Display for ScrobbleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "playback upload: {self:?}")
    }
}
impl std::error::Error for ScrobbleError {}

impl MusicApi {
    /// Snapshot the current login for ordered uploads owned by `account_id`.
    #[allow(unused)]
    pub fn scrobble_context(&self, account_id: u64) -> Result<ScrobbleContext, ScrobbleError> {
        let jar = self.cookie_jar().ok_or(ScrobbleError::Authentication)?;
        let uri = "https://music.163.com/"
            .parse()
            .map_err(|_| ScrobbleError::Encoding)?;
        let mut fields = BTreeMap::new();
        for name in [
            "MUSIC_U",
            "JSESSIONID-WYYY",
            "NMTID",
            "WEVNSM",
            "WNMCID",
            "__csrf",
            "_ntes_nnid",
            "_ntes_nuid",
            "clientSign",
            "deviceId",
            "mode",
            "vipType",
        ] {
            if let Some(cookie) = jar.get_by_name(&uri, name) {
                fields.insert(name.into(), cookie.value().to_owned());
            }
        }
        if account_id == 0 || fields.get("MUSIC_U").is_none_or(String::is_empty) {
            return Err(ScrobbleError::Authentication);
        }
        for (name, value) in [
            ("os", "pc"),
            (
                "osver",
                "Microsoft-Windows-10-Professional-build-19045-64bit",
            ),
            ("appver", VERSION),
            ("channel", "netease"),
            ("WEVNSM", "1.0.0"),
            ("_iuqxldmzr_", "33"),
            ("ntes_kaola_ad", "1"),
        ] {
            fields.entry(name.into()).or_insert_with(|| value.into());
        }
        fields.entry("WNMCID".into()).or_insert_with(|| {
            format!(
                "{:06x}.{}.01.0",
                rand::random::<u32>() & 0xffffff,
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
            )
        });
        for name in [
            "JSESSIONID-WYYY",
            "NMTID",
            "__csrf",
            "_ntes_nuid",
            "clientSign",
            "deviceId",
            "mode",
        ] {
            fields.entry(name.into()).or_default();
        }
        fields
            .entry("_ntes_nnid".into())
            .or_insert_with(|| ",".into());
        Ok(ScrobbleContext { account_id, fields })
    }

    /// Send one NCBL event using a frozen account context and the current proxy.
    ///
    /// # Errors
    /// Returns a typed encoding, transport, HTTP, business or acknowledgement outcome.
    #[allow(unused)]
    pub async fn scrobble(
        &self,
        context: &ScrobbleContext,
        event: &PlaybackEvent,
    ) -> Result<ScrobbleReceipt, ScrobbleError> {
        let upload = prepare(context, event).map_err(|_| ScrobbleError::Encoding)?;
        let (request, file_name) = upload_request(context, upload)?;
        let mut response = self
            .client
            .send_async(request)
            .await
            .map_err(|error| ScrobbleError::Transport(error.kind().clone()))?;
        if !response.status().is_success() {
            return Err(ScrobbleError::Http(response.status().as_u16()));
        }
        let text = response.text().await.map_err(|_| ScrobbleError::Unknown)?;
        let value = serde_json::from_str(&text).map_err(|_| ScrobbleError::Unknown)?;
        acknowledge(&value, &file_name)
    }
}

fn upload_request(
    context: &ScrobbleContext,
    upload: Upload,
) -> Result<(Request<Vec<u8>>, String), ScrobbleError> {
    let cookie = context
        .fields
        .iter()
        .filter(|(k, _)| k.as_str() != "vipType")
        .map(|(k, v)| format!("{k}={v}"))
        .chain(std::iter::once("__remember_me=true".to_owned()))
        .collect::<Vec<_>>()
        .join("; ");
    let request = Request::post(UPLOAD_URL)
        .timeout(Duration::from_secs(15))
        .redirect_policy(RedirectPolicy::None)
        .cookie_jar(crate::CookieJar::new())
        .header("Cookie", cookie)
        .header("Referer", "https://music.163.com/di")
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; WOW64) NeteaseMusicDesktop/3.1.35",
        )
        .header("Accept-Language", "zh-CN,zh;q=0.8")
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={}", upload.boundary),
        )
        .body(upload.body)
        .map_err(|_| ScrobbleError::Encoding)?;
    Ok((request, upload.file_name))
}

fn acknowledge(value: &Value, file_name: &str) -> Result<ScrobbleReceipt, ScrobbleError> {
    let code = value["code"].as_i64().ok_or(ScrobbleError::Unknown)?;
    if code == 200
        && value["data"]["successfiles"]
            .as_array()
            .is_some_and(|files| files.iter().any(|f| f.as_str() == Some(file_name)))
    {
        Ok(ScrobbleReceipt {
            file_name: file_name.into(),
        })
    } else if code == 200
        && value["data"]["failfiles"]
            .as_array()
            .is_none_or(|files| !files.iter().any(|f| f.as_str() == Some(file_name)))
    {
        Err(ScrobbleError::Unknown)
    } else {
        Err(ScrobbleError::Business {
            code,
            rate: value["data"]["rate"].as_i64(),
        })
    }
}

fn record(context: &ScrobbleContext, event: &PlaybackEvent) -> anyhow::Result<String> {
    anyhow::ensure!(event.song_id > 0, "song id is required");
    let timestamp = event.timestamp.duration_since(UNIX_EPOCH)?;
    let mut data = json!({
        "mode":"circulation", "download":0, "alg":"", "status":"front",
        "id":event.song_id.to_string(), "bitrate":event.bitrate / 1000,
        "type":"song", "is_listentogether":0, "source":event.source.name,
        "is_heart":0, "resource_ratio":"", "resource_time":event.duration.as_secs(),
        "bitrate_level":event.quality, "_addrefer":"", "_multirefers":[],
        "vipType":context.fields.get("vipType").cloned().unwrap_or_default(),
        "file":4, "rightSource":0, "sourceId":event.source.id, "sourcetype":event.source.kind,
        "libra_abt":"", "channel":"netease", "curStartChannel":""
    });
    let action = match event.kind {
        PlaybackEventKind::Start => {
            data["musiceffect_id"] = json!("");
            data["app_mode"] = json!(2);
            data["fee"] = json!(1);
            "_plv"
        }
        PlaybackEventKind::End => {
            let played = event.played.as_secs();
            data["time"] = json!(played);
            data["realtime"] = json!(played);
            data["musiceffect_id"] = json!("1001");
            data["app_mode"] = json!(1);
            data["lyriceffect"] = json!("default");
            data["displayMode"] = json!("classic");
            data["fee"] = json!(8);
            data["end"] = json!("interrupt");
            "_pld"
        }
    };
    Ok(format!(
        "{}\x01{action}\x01{}",
        timestamp.as_secs(),
        serde_json::to_string(&data)?
    ))
}

struct Upload {
    boundary: String,
    file_name: String,
    body: Vec<u8>,
}
fn prepare(context: &ScrobbleContext, event: &PlaybackEvent) -> anyhow::Result<Upload> {
    let mut key = [0; 32];
    let mut uuid = [0; 16];
    let mut seq = [0; 2];
    rand_bytes(&mut key)?;
    rand_bytes(&mut uuid)?;
    rand_bytes(&mut seq)?;
    uuid[6] = (uuid[6] & 15) | 0x40;
    uuid[8] = (uuid[8] & 63) | 0x80;
    let fields: BTreeMap<_, _> = context
        .fields
        .iter()
        .filter(|(key, _)| key.as_str() != "vipType")
        .collect();
    let meta = serde_json::to_vec(&fields)?;
    let body = record(context, event)?;
    let payload = encode(&meta, body.as_bytes(), key, uuid, u16::from_le_bytes(seq))?;
    let boundary = hex::encode(uuid);
    let file_name = format!(
        "op_{}_0_{}",
        u16::from_le_bytes(seq) as u32 + 10000,
        rand::random::<u32>()
    );
    let mut body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: multipart/form-data\r\n\r\n").into_bytes();
    body.extend(payload);
    write!(body, "\r\n--{boundary}--\r\n")?;
    Ok(Upload {
        boundary,
        file_name,
        body,
    })
}

fn wrap(key: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
    let n =
        BigNum::from_hex_str("fd90bd466ff9bc8a3fec2fbcf263b90d5c564879fa5d7aab89b31c1d5cb4139d")?;
    let e = BigNum::from_u32(65537)?;
    let a = BigNum::from_slice(key)?;
    let mut out = BigNum::new()?;
    let mut context = BigNumContext::new()?;
    out.mod_exp(&a, &e, &n, &mut context)?;
    Ok(out.to_vec_padded(32)?)
}

fn crypt(data: &[u8], key: &[u8], uuid: &[u8; 16]) -> anyhow::Result<Vec<u8>> {
    let mut iv = [0u8; 16];
    let counter = u32::from_le_bytes(uuid[12..16].try_into()?) >> 2;
    iv[..4].copy_from_slice(&counter.to_le_bytes());
    iv[4..].copy_from_slice(&uuid[..12]);
    let mut cipher = Crypter::new(Cipher::chacha20(), Mode::Encrypt, key, Some(&iv))?;
    let mut out = vec![0; data.len() + Cipher::chacha20().block_size()];
    let size = cipher.update(data, &mut out)?;
    let end = cipher.finalize(&mut out[size..])?;
    out.truncate(size + end);
    Ok(out)
}

fn encode(
    meta: &[u8],
    body: &[u8],
    mut key: [u8; 32],
    uuid: [u8; 16],
    seq: u16,
) -> anyhow::Result<Vec<u8>> {
    if key[0] >= 0xa3 {
        key[0] = 0xa2;
    }
    let wrapped = wrap(&key)?;
    let meta = crypt(meta, &wrapped, &uuid)?;
    let header_len = u16::try_from(74 + meta.len())?;
    let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
    gzip.write_all(body)?;
    let compressed = gzip.finish()?;
    let mut frames = Vec::new();
    let mut last = u32::from(seq);
    for (i, chunk) in compressed.chunks(0x8000).enumerate() {
        last = u32::from(seq)
            .checked_add(u32::try_from(i)?)
            .ok_or_else(|| anyhow::anyhow!("sequence overflow"))?;
        frames.extend(u16::try_from(chunk.len())?.to_le_bytes());
        frames.extend(last.to_le_bytes());
        frames.extend(crypt(chunk, &key, &uuid)?);
    }
    let mut out = Vec::new();
    out.extend(b"NCBL");
    out.extend(3u32.to_le_bytes());
    out.extend(header_len.to_le_bytes());
    out.extend(uuid);
    out.extend(wrapped);
    out.extend(u32::from(seq).to_le_bytes());
    out.extend(last.to_le_bytes());
    out.extend(u32::try_from(frames.len())?.to_le_bytes());
    out.extend(0x4343u16.to_le_bytes());
    out.extend(u16::try_from(meta.len())?.to_le_bytes());
    out.extend(meta);
    out.extend(frames);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn login_snapshot_and_multipart_bind_one_upload() {
        let jar = crate::CookieJar::new();
        let uri = "https://music.163.com/".parse().unwrap();
        let cookie = crate::CookieBuilder::new("MUSIC_U", "fixture+token")
            .domain("music.163.com")
            .path("/")
            .build()
            .unwrap();
        jar.set(cookie, &uri).unwrap();
        let api = MusicApi::from_cookie_jar(jar, 1);
        let context = api.scrobble_context(7).unwrap();
        assert_eq!(context.fields["MUSIC_U"], "fixture+token");
        assert_eq!(context.fields["os"], "pc");
        let event = PlaybackEvent {
            session_id: 1,
            kind: PlaybackEventKind::Start,
            song_id: 42,
            duration: Duration::from_secs(180),
            played: Duration::ZERO,
            source: PlaybackSource::default(),
            bitrate: 0,
            quality: String::new(),
            timestamp: UNIX_EPOCH + Duration::from_secs(100),
        };
        let upload = prepare(&context, &event).unwrap();
        let header = format!("--{}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\nContent-Type: multipart/form-data\r\n\r\n", upload.boundary, upload.file_name);
        assert!(upload.body.starts_with(header.as_bytes()));
        assert_eq!(&upload.body[header.len()..header.len() + 4], b"NCBL");
        assert!(upload
            .body
            .ends_with(format!("\r\n--{}--\r\n", upload.boundary).as_bytes()));
        api.cookie_jar()
            .unwrap()
            .set(
                crate::CookieBuilder::new("MUSIC_U", "next-account")
                    .domain("music.163.com")
                    .path("/")
                    .build()
                    .unwrap(),
                &uri,
            )
            .unwrap();
        let (request, file_name) = upload_request(&context, upload).unwrap();
        assert_eq!(request.uri(), UPLOAD_URL);
        assert_eq!(request.method(), isahc::http::Method::POST);
        assert!(request.headers()["Cookie"]
            .to_str()
            .unwrap()
            .contains("MUSIC_U=fixture+token"));
        assert!(request.headers()["Content-Type"]
            .to_str()
            .unwrap()
            .starts_with("multipart/form-data; boundary="));
        assert!(file_name.starts_with("op_"));
    }
    #[test]
    fn desktop_record_preserves_real_duration_and_origin() {
        let context = ScrobbleContext {
            account_id: 1,
            fields: BTreeMap::new(),
        };
        let event = PlaybackEvent {
            session_id: 8,
            kind: PlaybackEventKind::End,
            song_id: 42,
            duration: Duration::from_secs(180),
            played: Duration::from_micros(12_900_000),
            source: PlaybackSource {
                id: "7".into(),
                kind: "list".into(),
                name: "list".into(),
            },
            bitrate: 320000,
            quality: "exhigh".into(),
            timestamp: UNIX_EPOCH + Duration::from_secs(100),
        };
        let record = record(&context, &event).unwrap();
        let fields: Vec<_> = record.split('\x01').collect();
        assert_eq!(&fields[..2], &["100", "_pld"]);
        let data: Value = serde_json::from_str(fields[2]).unwrap();
        assert_eq!(
            (data["time"].as_u64(), data["resource_time"].as_u64()),
            (Some(12), Some(180))
        );
        assert_eq!(data["sourceId"], "7");
        assert_eq!(data["bitrate"], 320);
        let receipt = acknowledge(
            &json!({"code":200,"data":{"successfiles":["op_1"]}}),
            "op_1",
        )
        .unwrap();
        assert_eq!(receipt.file_name, "op_1");
    }

    #[test]
    fn ncbl_header_and_roundtrip_match_desktop_layout() {
        let key = [1u8; 32];
        let uuid = [2u8; 16];
        let payload = encode(b"{}", b"100\x01_plv\x01{}", key, uuid, 7).unwrap();
        assert_eq!(&payload[..10], b"NCBL\x03\0\0\0L\0");
        assert_eq!(&payload[58..66], &[7, 0, 0, 0, 7, 0, 0, 0]);
        let wrapped = wrap(&key).unwrap();
        assert_eq!(
            hex::encode(&wrapped),
            "bd363d63a8543be3c6aa2e138f731ca00dd536af4ced6381c449150c43014656"
        );
        assert_eq!(hex::encode(&payload[74..76]), "81c3");
        assert_eq!(crypt(&payload[74..76], &wrapped, &uuid).unwrap(), b"{}");
        let gzip = crypt(&payload[82..], &key, &uuid).unwrap();
        let mut decoder = flate2::read::GzDecoder::new(&gzip[..]);
        let mut body = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut body).unwrap();
        assert_eq!(body, "100\x01_plv\x01{}");
    }
}
