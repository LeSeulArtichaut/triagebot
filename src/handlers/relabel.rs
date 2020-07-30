//! Purpose: Allow any user to modify issue labels on GitHub via comments.
//!
//! Labels are checked against the labels in the project; the bot does not support creating new
//! labels.
//!
//! Parsing is done in the `parser::command::relabel` module.
//!
//! If the command was successful, there will be no feedback beyond the label change to reduce
//! notification noise.

use crate::{
    zulip,
    config::{self, RelabelConfig},
    github::{self, Event, Issue, GithubClient, is_team_member_id},
    handlers::{Context, GithubHandler, ZulipHandler},
    interactions::ErrorComment,
    zulip::Request,
};
use futures::future::{BoxFuture, FutureExt};
use parser::command::relabel::{LabelDelta, RelabelCommand};
use parser::command::{Command, Input};

pub(super) struct RelabelHandler;

impl GithubHandler for RelabelHandler {
    type Input = RelabelCommand;
    type Config = RelabelConfig;

    fn parse_input(
        &self,
        ctx: &Context,
        event: &Event,
        _: Option<&Self::Config>,
    ) -> Result<Option<Self::Input>, String> {
        let body = if let Some(b) = event.comment_body() {
            b
        } else {
            // not interested in other events
            return Ok(None);
        };

        if let Event::Issue(e) = event {
            if e.action != github::IssuesAction::Opened {
                // skip events other than opening the issue to avoid retriggering commands in the
                // issue body
                return Ok(None);
            }
        }

        let mut input = Input::new(&body, &ctx.gh_username);
        match input.parse_github_command() {
            Command::Relabel(Ok(command)) => Ok(Some(command)),
            Command::Relabel(Err(err)) => {
                return Err(format!(
                    "Parsing label command in [comment]({}) failed: {}",
                    event.html_url().expect("has html url"),
                    err
                ));
            }
            _ => Ok(None),
        }
    }

    fn handle_input<'a>(
        &self,
        ctx: &'a Context,
        config: &'a RelabelConfig,
        event: &'a Event,
        input: RelabelCommand,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        handle_github_input(ctx, config, event, input).boxed()
    }
}

impl ZulipHandler for RelabelHandler {
    type Input = RelabelCommand;

    fn parse_input(&self, ctx: &Context, req: &Request) -> Result<Option<Self::Input>, String> {
        let mut input = Input::new(&req.data, &ctx.zulip_username);
        match input.parse_zulip_command(req.message.type_ == "private") {
            Command::Relabel(Ok(command)) => Ok(Some(command)),
            Command::Relabel(Err(err)) => {
                return Err(format!("Parsing label command failed: {}", err));
            }
            _ => Ok(None),
        }
    }

    fn handle_input<'a>(&self, ctx: &'a Context, req: &'a Request, input: Self::Input) -> BoxFuture<'a, anyhow::Result<()>> {
        handle_zulip_input(ctx, req, input).boxed()
    }
}

async fn handle_github_input(
    ctx: &Context,
    config: &RelabelConfig,
    event: &Event,
    input: RelabelCommand,
) -> anyhow::Result<()> {
    let membership = is_member(event.user(), &ctx.github).await;
    handle_input(&ctx, &config, &event.issue().unwrap(), membership, input).await
}

async fn handle_zulip_input(
    ctx: &Context,
    req: &Request,
    input: RelabelCommand,
) -> anyhow::Result<()> {
    let issue_ref = match &input.1 {
        Some(issue_ref) => issue_ref,
        None => {
            req.message.reply(&ctx.github.raw(), "No issue reference given").await?;
            return Ok(());
        }
    };
    let issue = Issue::get_issue(&ctx.github, &issue_ref.repository, issue_ref.number).await?;
    let repo_config = config::get(&ctx.github, &issue_ref.repository).await?;
    let config = repo_config
        .as_ref()
        .relabel
        .as_ref();
    let config = match config {
        Some(c) => c,
        None => {
            req.message.reply(
                &ctx.github.raw(),
                &format!("The feature `relabel` is not enabled in the `{}` repository.", &issue_ref.repository),
            ).await?;
            return Ok(());
        }
    };
    let github_id = zulip::to_github_id(&ctx.github, req.message.sender_id as usize).await?;
    let membership = match github_id {
        Some(id) => is_member_by_id(id, &ctx.github).await,
        None => TeamMembership::Unknown,
    };

    handle_input(&ctx, &config, &issue, membership, input).await
}

async fn handle_input(
    ctx: &Context,
    config: &RelabelConfig,
    issue: &Issue,
    membership: TeamMembership,
    input: RelabelCommand,
) -> anyhow::Result<()> {
    let mut issue_labels = issue.labels().to_owned();
    let mut changed = false;
    for delta in &input.0 {
        let name = delta.label().as_str();
        let err = match check_filter(name, config, membership) {
            Ok(CheckFilterResult::Allow) => None,
            Ok(CheckFilterResult::Deny) => Some(format!(
                "Label {} can only be set by Rust team members",
                name
            )),
            Ok(CheckFilterResult::DenyUnknown) => Some(format!(
                "Label {} can only be set by Rust team members;\
                 we were unable to check if you are a team member.",
                name
            )),
            Err(err) => Some(err),
        };
        if let Some(msg) = err {
            let cmnt = ErrorComment::new(&issue, msg);
            cmnt.post(&ctx.github).await?;
            return Ok(());
        }
        match delta {
            LabelDelta::Add(label) => {
                if !issue_labels.iter().any(|l| l.name == label.as_str()) {
                    changed = true;
                    issue_labels.push(github::Label {
                        name: label.to_string(),
                    });
                }
            }
            LabelDelta::Remove(label) => {
                if let Some(pos) = issue_labels.iter().position(|l| l.name == label.as_str()) {
                    changed = true;
                    issue_labels.remove(pos);
                }
            }
        }
    }

    if changed {
        issue.set_labels(&ctx.github, issue_labels).await?;
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum TeamMembership {
    Member,
    Outsider,
    Unknown,
}

async fn is_member(user: &github::User, client: &GithubClient) -> TeamMembership {
    match user.is_team_member(client).await {
        Ok(true) => TeamMembership::Member,
        Ok(false) => TeamMembership::Outsider,
        Err(err) => {
            eprintln!("failed to check team membership: {:?}", err);
            TeamMembership::Unknown
        }
    }
}

async fn is_member_by_id(user_id: i64, client: &GithubClient) -> TeamMembership {
    match is_team_member_id(user_id as usize, &client).await {
        Ok(true) => TeamMembership::Member,
        Ok(false) => TeamMembership::Outsider,
        Err(err) => {
            eprintln!("failed to check team membership: {:?}", err);
            TeamMembership::Unknown
        }
    }
}

#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
enum CheckFilterResult {
    Allow,
    Deny,
    DenyUnknown,
}

fn check_filter(
    label: &str,
    config: &RelabelConfig,
    is_member: TeamMembership,
) -> Result<CheckFilterResult, String> {
    if is_member == TeamMembership::Member {
        return Ok(CheckFilterResult::Allow);
    }
    let mut matched = false;
    for pattern in &config.allow_unauthenticated {
        match match_pattern(pattern, label) {
            Ok(MatchPatternResult::Allow) => matched = true,
            Ok(MatchPatternResult::Deny) => {
                // An explicit deny overrides any allowed pattern
                matched = false;
                break;
            }
            Ok(MatchPatternResult::NoMatch) => {}
            Err(err) => {
                eprintln!("failed to match pattern {}: {}", pattern, err);
                return Err(format!("failed to match pattern {}", pattern));
            }
        }
    }
    if matched {
        return Ok(CheckFilterResult::Allow);
    } else if is_member == TeamMembership::Outsider {
        return Ok(CheckFilterResult::Deny);
    } else {
        return Ok(CheckFilterResult::DenyUnknown);
    }
}

#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
enum MatchPatternResult {
    Allow,
    Deny,
    NoMatch,
}

fn match_pattern(pattern: &str, label: &str) -> anyhow::Result<MatchPatternResult> {
    let (pattern, inverse) = if pattern.starts_with('!') {
        (&pattern[1..], true)
    } else {
        (pattern, false)
    };
    let glob = glob::Pattern::new(pattern)?;
    Ok(match (glob.matches(label), inverse) {
        (true, false) => MatchPatternResult::Allow,
        (true, true) => MatchPatternResult::Deny,
        (false, _) => MatchPatternResult::NoMatch,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        check_filter, match_pattern, CheckFilterResult, MatchPatternResult, TeamMembership,
    };
    use crate::config::RelabelConfig;

    #[test]
    fn test_match_pattern() -> anyhow::Result<()> {
        assert_eq!(
            match_pattern("I-*", "I-nominated")?,
            MatchPatternResult::Allow
        );
        assert_eq!(
            match_pattern("!I-no*", "I-nominated")?,
            MatchPatternResult::Deny
        );
        assert_eq!(
            match_pattern("I-*", "T-infra")?,
            MatchPatternResult::NoMatch
        );
        assert_eq!(
            match_pattern("!I-no*", "T-infra")?,
            MatchPatternResult::NoMatch
        );
        Ok(())
    }

    #[test]
    fn test_check_filter() -> anyhow::Result<()> {
        macro_rules! t {
            ($($member:ident { $($label:expr => $res:ident,)* })*) => {
                let config = RelabelConfig {
                    allow_unauthenticated: vec!["T-*".into(), "I-*".into(), "!I-nominated".into()],
                };
                $($(assert_eq!(
                    check_filter($label, &config, TeamMembership::$member),
                    Ok(CheckFilterResult::$res)
                );)*)*
            }
        }
        t! {
            Member {
                "T-release" => Allow,
                "I-slow" => Allow,
                "I-nominated" => Allow,
                "A-spurious" => Allow,
            }
            Outsider {
                "T-release" => Allow,
                "I-slow" => Allow,
                "I-nominated" => Deny,
                "A-spurious" => Deny,
            }
            Unknown {
                "T-release" => Allow,
                "I-slow" => Allow,
                "I-nominated" => DenyUnknown,
                "A-spurious" => DenyUnknown,
            }
        }
        Ok(())
    }
}
