use crate::{
    zulip,
    config::{self, RelabelConfig},
    db::notifications::{self, add_metadata, delete_ping, move_indices, record_ping, Identifier},
    github::{self, Event, Issue, GithubClient, is_team_member_id},
    handlers::{Context, GithubHandler, ZulipHandler},
    interactions::ErrorComment,
    zulip::Request,
};
use futures::future::{BoxFuture, FutureExt};
use parser::command::manage_notifs::{NotifCommand, NotifCommandKind};
use parser::command::{Command, Input};
pub(super) struct NotificationHandler;

impl ZulipHandler for NotificationHandler {
    type Input = NotifCommand;

    fn parse_input(&self, ctx: &Context, req: &Request) -> Result<Option<Self::Input>, String> {
        let mut input = Input::new(&req.data, &ctx.zulip_username);
        match input.parse_zulip_command(req.message.type_ == "private") {
            Command::HandleNotifs(Ok(command)) => Ok(Some(command)),
            Command::HandleNotifs(Err(err)) => {
                return Err(format!("Parsing label command failed: {}", err));
            }
            _ => Ok(None),
        }
    }

    fn handle_input<'a>(&self, ctx: &'a Context, req: &'a Request, input: Self::Input) -> BoxFuture<'a, anyhow::Result<()>> {
        handle_input(ctx, req, input).boxed()
    }
}

async fn handle_input(
    ctx: &Context,
    req: &Request,
    input: NotifCommand,
) -> anyhow::Result<()> {
    if let Some("as") = next {
        return match execute_for_other_user(&ctx, words, message_data).await {
            Ok(r) => r,
            Err(e) => serde_json::to_string(&Response {
                content: &format!(
                    "Failed to parse; expected `as <username> <command...>`: {:?}.",
                    e
                ),
            })
            .unwrap(),
        };
    }

    let gh_id = match zulip::to_github_id(&ctx.github, req.message.sender_id as usize).await {
        Ok(Some(gh_id)) => gh_id,
        Ok(None) => {
            req.message.reply(&ctx.github.raw(), &format!(
                "Unknown Zulip user. Please add `zulip-id = {}` to your file in rust-lang/team.",
                req.message.sender_id
            )).await?;
            return Ok(());
        }
        Err(e) => {
            req.message.reply(
                &ctx.github.raw(),
                &format!("Failed to query team API: {:?}", e),
            ).await?;
            return Ok(());
        }
    };

    match input.command {
        NotifCommandKind::Acknowledge(idx) => acknowledge(req, gh_id, idx),
        NotifCommandKind::Add(url, description) => add_notification(&ctx, gh_id, url, description),
        NotifCommandKind::Move(from, to) => move_notification(gh_id, from, to),
        NotifCommandKind::Meta(idx, metadata) => add_meta_notification(gh_id, idx, metadata),
    }
}

// This does two things:
//  * execute the command for the other user
//  * tell the user executed for that a command was run as them by the user
//    given.
async fn execute_for_other_user(
    ctx: &Context,
    req: &Request,
    input: NotifCommand,
) -> anyhow::Result<String> {
    // username is a GitHub username, not a Zulip username
    let username = match words.next() {
        Some(username) => username,
        None => anyhow::bail!("no username provided"),
    };
    let user_id = match (github::User {
        login: username.to_owned(),
        id: None,
    })
    .get_id(&ctx.github)
    .await
    .context("getting ID of github user")?
    {
        Some(id) => id.try_into().unwrap(),
        None => {
            return Ok(serde_json::to_string(&Response {
                content: "Can only authorize for other GitHub users.",
            })
            .unwrap());
        }
    };
    let mut command = words.fold(String::new(), |mut acc, piece| {
        acc.push_str(piece);
        acc.push(' ');
        acc
    });
    let command = if command.is_empty() {
        anyhow::bail!("no command provided")
    } else {
        assert_eq!(command.pop(), Some(' ')); // pop trailing space
        command
    };
    let bot_api_token = env::var("ZULIP_API_TOKEN").expect("ZULIP_API_TOKEN");

    let members = ctx
        .github
        .raw()
        .get("https://rust-lang.zulipchat.com/api/v1/users")
        .basic_auth(BOT_EMAIL, Some(&bot_api_token))
        .send()
        .await;
    let members = match members {
        Ok(members) => members,
        Err(e) => {
            return Ok(serde_json::to_string(&Response {
                content: &format!("Failed to get list of zulip users: {:?}.", e),
            })
            .unwrap());
        }
    };
    let members = members.json::<MembersApiResponse>().await;
    let members = match members {
        Ok(members) => members.members,
        Err(e) => {
            return Ok(serde_json::to_string(&Response {
                content: &format!("Failed to get list of zulip users: {:?}.", e),
            })
            .unwrap());
        }
    };

    // Map GitHub `user_id` to `zulip_user_id`.
    let zulip_user_id = match to_zulip_id(&ctx.github, user_id).await {
        Ok(Some(id)) => id as u64,
        Ok(None) => {
            return Ok(serde_json::to_string(&Response {
                content: &format!("Could not find Zulip ID for GitHub ID: {}", user_id),
            })
            .unwrap());
        }
        Err(e) => {
            return Ok(serde_json::to_string(&Response {
                content: &format!("Could not find Zulip ID for GitHub id {}: {:?}", user_id, e),
            })
            .unwrap());
        }
    };

    let user = match members.iter().find(|m| m.user_id == zulip_user_id) {
        Some(m) => m,
        None => {
            return Ok(serde_json::to_string(&Response {
                content: &format!("Could not find Zulip user email."),
            })
            .unwrap());
        }
    };

    let output = handle_command(ctx, Ok(user_id as i64), &command, message_data).await;
    let output_msg: ResponseOwned =
        serde_json::from_str(&output).expect("result should always be JSON");
    let output_msg = output_msg.content;

    // At this point, the command has been run (FIXME: though it may have
    // errored, it's hard to determine that currently, so we'll just give the
    // output fromt he command as well as the command itself).

    let message = format!(
        "{} ({}) ran `{}` with output `{}` as you.",
        message_data.sender_full_name, message_data.sender_short_name, command, output_msg
    );

    let res = MessageApiRequest {
        recipient: Recipient::Private {
            id: user.user_id,
            email: &user.email,
        },
        content: &message
    }
    .send(ctx.github.raw())
    .await;

    match res {
        Ok(resp) => {
            if !resp.status().is_success() {
                log::error!(
                    "Failed to notify real user about command: response: {:?}",
                    resp
                );
            }
        }
        Err(err) => {
            log::error!("Failed to notify real user about command: {:?}", err);
        }
    }

    Ok(output)
}

async fn acknowledge(gh_id: i64, idx: String) -> anyhow::Result<String> {
    let url = match words.next() {
        Some(url) => {
            if words.next().is_some() {
                anyhow::bail!("too many words");
            }
            url
        }
        None => anyhow::bail!("not enough words"),
    };
    let ident = if let Ok(number) = url.parse::<usize>() {
        Identifier::Index(
            std::num::NonZeroUsize::new(number)
                .ok_or_else(|| anyhow::anyhow!("index must be at least 1"))?,
        )
    } else {
        Identifier::Url(url)
    };
    match delete_ping(&mut crate::db::make_client().await?, gh_id, ident).await {
        Ok(deleted) => {
            let mut resp = format!("Acknowledged:\n");
            for deleted in deleted {
                resp.push_str(&format!(
                    " * [{}]({}){}\n",
                    deleted
                        .short_description
                        .as_deref()
                        .unwrap_or(&deleted.origin_url),
                    deleted.origin_url,
                    deleted
                        .metadata
                        .map_or(String::new(), |m| format!(" ({})", m)),
                ));
            }
            Ok(serde_json::to_string(&Response { content: &resp }).unwrap())
        }
        Err(e) => Ok(serde_json::to_string(&Response {
            content: &format!("Failed to acknowledge {}: {:?}.", url, e),
        })
        .unwrap()),
    }
}

async fn add_notification(
    ctx: &Context,
    req: &Request,
    gh_id: i64,
    url: String,
    description: String,
) -> anyhow::Result<String> {
    let description = if description.is_empty() {
        None
    } else {
        Some(description)
    };
    match record_ping(
        &ctx.db,
        &notifications::Notification {
            user_id: gh_id,
            origin_url: url.to_owned(),
            origin_html: String::new(),
            short_description: description,
            time: chrono::Utc::now().into(),
            team_name: None,
        },
    )
    .await
    {
        Ok(()) => Ok(serde_json::to_string(&Response {
            content: "Created!",
        })
        .unwrap()),
        Err(e) => Ok(serde_json::to_string(&Response {
            content: &format!("Failed to create: {:?}", e),
        })
        .unwrap()),
    }
}

async fn add_meta_notification(
    gh_id: i64,
    idx: String,
    metadata: String,
) -> anyhow::Result<String> {
    let idx = match words.next() {
        Some(idx) => idx,
        None => anyhow::bail!("idx not present"),
    };
    let idx = idx
        .parse::<usize>()
        .context("index")?
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("1-based indexes"))?;
    let mut description = words.fold(String::new(), |mut acc, piece| {
        acc.push_str(piece);
        acc.push(' ');
        acc
    });
    let description = if description.is_empty() {
        None
    } else {
        assert_eq!(description.pop(), Some(' ')); // pop trailing space
        Some(description)
    };
    match add_metadata(
        &mut crate::db::make_client().await?,
        gh_id,
        idx,
        description.as_deref(),
    )
    .await
    {
        Ok(()) => Ok(serde_json::to_string(&Response {
            content: "Added metadata!",
        })
        .unwrap()),
        Err(e) => Ok(serde_json::to_string(&Response {
            content: &format!("Failed to add: {:?}", e),
        })
        .unwrap()),
    }
}

async fn move_notification(
    gh_id: i64,
    from: String,
    to: String,
) -> anyhow::Result<String> {
    let from = match words.next() {
        Some(idx) => idx,
        None => anyhow::bail!("from idx not present"),
    };
    let to = match words.next() {
        Some(idx) => idx,
        None => anyhow::bail!("from idx not present"),
    };
    let from = from
        .parse::<usize>()
        .context("from index")?
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("1-based indexes"))?;
    let to = to
        .parse::<usize>()
        .context("to index")?
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("1-based indexes"))?;
    match move_indices(&mut crate::db::make_client().await?, gh_id, from, to).await {
        Ok(()) => Ok(serde_json::to_string(&Response {
            // to 1-base indices
            content: &format!("Moved {} to {}.", from + 1, to + 1),
        })
        .unwrap()),
        Err(e) => Ok(serde_json::to_string(&Response {
            content: &format!("Failed to move: {:?}.", e),
        })
        .unwrap()),
    }
}
