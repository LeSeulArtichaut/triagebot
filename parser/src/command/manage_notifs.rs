use crate::error::Error;
use crate::token::{Token, Tokenizer};

#[derive(Debug)]
pub struct NotifCommand {
    pub command: NotifCommandKind,
    pub user_override: Option<String>,
}

#[derive(Debug)]
pub enum NotifCommandKind {
    Acknowledge(String),
    Add(String, String),
    Move(String, String),
    Meta(String, String),
}

impl NotifCommand {
    pub fn parse<'a>(input: &mut Tokenizer<'a>) -> Result<Option<Self>, Error<'a>> {
        let mut toks = input.clone();
        let mut user_override = None;
        if let Some(Token::Word("as")) = toks.peek_token()? {
            toks.next_token()?;
            if let Some(Token::Word(user)) = toks.next_token()? {
                user_override = Some(user.to_owned());
            } else {
                return Ok(None);
            }
        }
        let command = if let Some(Token::Word(cmd)) = toks.peek_token()? {
            match cmd {
                "acknowledge" | "ack" => {
                    let idx = match toks.next_token()? {
                        Some(Token::Word(idx)) => idx,
                        Some(Token::Quote(url)) => url,
                        _ => return Ok(None),
                    };
                    NotifCommandKind::Acknowledge(idx.to_owned())
                },
                "add" => {
                    let url = if let Some(Token::Quote(url)) = toks.next_token()? {
                        url.to_owned()
                    } else {
                        return Ok(None);
                    };
                    let mut description = String::new();
                    loop {
                        if let Some(Token::Semi) | Some(Token::Dot) | Some(Token::EndOfLine) =
                            toks.peek_token()?
                        {
                            description.pop();
                            break NotifCommandKind::Add(url, description);
                        }
                        if toks.peek_token()? == None {
                            description.pop();
                            break NotifCommandKind::Add(url, description);
                        }
                        description.push_str(&toks.next_token()?.unwrap().to_string());
                        description.push(' ');
                    }
                },
                "move" => {
                    let from = match toks.next_token()? {
                        Some(Token::Word(idx)) => idx,
                        Some(Token::Quote(url)) => url,
                        _ => return Ok(None),
                    };
                    let to = match toks.next_token()? {
                        Some(Token::Word(idx)) => idx,
                        Some(Token::Quote(url)) => url,
                        _ => return Ok(None),
                    };
                    NotifCommandKind::Move(from.to_owned(), to.to_owned())
                },
                "meta" => {
                    let idx = if let Some(Token::Word(idx)) = toks.next_token()? {
                        idx.to_owned()
                    } else {
                        return Ok(None);
                    };
                    let mut description = String::new();
                    loop {
                        if let Some(Token::Semi) | Some(Token::Dot) | Some(Token::EndOfLine) =
                            toks.peek_token()?
                        {
                            description.pop();
                            break NotifCommandKind::Add(idx, description);
                        }
                        if toks.peek_token()? == None {
                            description.pop();
                            break NotifCommandKind::Add(idx, description);
                        }
                        description.push_str(&toks.next_token()?.unwrap().to_string());
                        description.push(' ');
                    }
                }
                _ => return Ok(None),
            }
        } else {
            return Ok(None);
        };
        Ok(Some(NotifCommand {
            command,
            user_override,
        }))
    }
}
