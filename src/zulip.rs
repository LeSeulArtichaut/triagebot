use crate::github::{self, GithubClient};
use crate::handlers::{self, Context, HandlerError};
use anyhow::Context as _;
use std::convert::TryInto;
use std::env;
use std::io::Write as _;

#[derive(Debug, serde::Deserialize)]
pub struct Request {
    /// Markdown body of the sent message.
    pub data: String,

    /// Metadata about this request.
    pub message: Message,

    /// Authentication token. The same for all Zulip messages.
    token: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct Message {
    pub sender_id: u64,
    pub sender_email: String,
    pub recipient_id: u64,
    pub sender_short_name: String,
    pub sender_full_name: String,
    pub stream_id: Option<u64>,
    pub topic: Option<String>,
    #[serde(rename = "type")]
    pub type_: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Response<'a> {
    content: &'a str,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ResponseOwned {
    content: String,
}

pub const BOT_EMAIL: &str = "triage-rust-lang-bot@zulipchat.com";

impl Message {
    pub async fn reply(&self, client: &reqwest::Client, content: &str) -> anyhow::Result<()> {
        let recipient = match &*self.type_ {
            "private" => Recipient::Private {
                email: &self.sender_email,
                id: self.recipient_id,
            },
            "stream" => Recipient::Stream {
                id: self.recipient_id,
                topic: self.topic.as_ref().unwrap(),
            },
            _ => panic!("Unknown message type: {}", &self.type_)
        };
        MessageApiRequest {
            recipient,
            content,
        }
        .send(client)
        .await?;
        Ok(())
    }
}

pub async fn to_github_id(client: &GithubClient, zulip_id: usize) -> anyhow::Result<Option<i64>> {
    let map = crate::team_data::zulip_map(client).await?;
    Ok(map.users.get(&zulip_id).map(|v| *v as i64))
}

pub async fn to_zulip_id(client: &GithubClient, github_id: i64) -> anyhow::Result<Option<usize>> {
    let map = crate::team_data::zulip_map(client).await?;
    Ok(map
        .users
        .iter()
        .find(|(_, github)| **github == github_id as usize)
        .map(|v| *v.0))
}

pub async fn respond(ctx: &Context, req: Request) -> String {
    let expected_token = std::env::var("ZULIP_TOKEN").expect("`ZULIP_TOKEN` set for authorization");

    if !openssl::memcmp::eq(req.token.as_bytes(), expected_token.as_bytes()) {
        return serde_json::to_string(&Response {
            content: "Invalid authorization.",
        })
        .unwrap();
    }

    log::trace!("zulip hook: {:?}", req);

    if let Err(err) = handlers::handle_zulip(&ctx, &req).await {
        match err {
            HandlerError::Message(message) => {
                return serde_json::to_string(&Response {
                    content: &message
                }).unwrap();
            },
            HandlerError::Other(err) => {
                log::error!("handling zulip event failed: {:?}", err);
                return serde_json::to_string(&Response {
                    content: "handling failed, error logged"
                }).unwrap();
            },
        }
    };

    String::new() // FIXME: what to send back in case of success?
}

#[derive(serde::Deserialize)]
struct MembersApiResponse {
    members: Vec<Member>,
}

#[derive(serde::Deserialize)]
struct Member {
    email: String,
    user_id: u64,
}

#[derive(serde::Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum Recipient<'a> {
    Stream {
        #[serde(rename = "to")]
        id: u64,
        topic: &'a str,
    },
    Private {
        #[serde(skip)]
        id: u64,
        #[serde(rename = "to")]
        email: &'a str,
    },
}

impl Recipient<'_> {
    pub fn narrow(&self) -> String {
        match self {
            Recipient::Stream { id, topic } => {
                // See
                // https://github.com/zulip/zulip/blob/46247623fc279/zerver/lib/url_encoding.py#L9
                // ALWAYS_SAFE without `.` from
                // https://github.com/python/cpython/blob/113e2b0a07c/Lib/urllib/parse.py#L772-L775
                //
                // ALWAYS_SAFE doesn't contain `.` because Zulip actually encodes them to be able
                // to use `.` instead of `%` in the encoded strings
                const ALWAYS_SAFE: &str =
                    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-~";

                let mut encoded_topic = Vec::new();
                for ch in topic.bytes() {
                    if !(ALWAYS_SAFE.contains(ch as char)) {
                        write!(encoded_topic, "%{:02X}", ch).unwrap();
                    } else {
                        encoded_topic.push(ch);
                    }
                }

                let mut encoded_topic = String::from_utf8(encoded_topic).unwrap();
                encoded_topic = encoded_topic.replace("%", ".");

                format!("stream/{}-xxx/topic/{}", id, encoded_topic)
            }
            Recipient::Private { id, .. } => format!("pm-with/{}-xxx", id),
        }
    }
}

#[derive(serde::Serialize)]
pub struct MessageApiRequest<'a> {
    pub recipient: Recipient<'a>,
    pub content: &'a str,
}

impl<'a> MessageApiRequest<'a> {
    pub fn url(&self) -> String {
        format!(
            "https://rust-lang.zulipchat.com/#narrow/{}",
            self.recipient.narrow()
        )
    }

    pub async fn send(&self, client: &reqwest::Client) -> anyhow::Result<reqwest::Response> {
        let bot_api_token = env::var("ZULIP_API_TOKEN").expect("ZULIP_API_TOKEN");

        Ok(client
            .post("https://rust-lang.zulipchat.com/api/v1/messages")
            .basic_auth(BOT_EMAIL, Some(&bot_api_token))
            .form(&self)
            .send()
            .await?)
    }
}
