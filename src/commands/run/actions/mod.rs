//! Action execution for the `run` command.
//!
//! [`execute_action`] dispatches one configured [`Action`] at a time and threads a
//! [`Carry`] between consecutive actions, so a `click` can read the message sent by
//! a preceding `text`/`dice`, and `browserless` can consume the URL opened by a
//! preceding `click`.

mod browserless;
mod click;
mod llm;
mod miniapp;

use crate::config::Action;
use anyhow::Result;
use grammers_client::Client;
use grammers_client::message::{InputMessage, Message};
use grammers_client::session::types::PeerRef;
use grammers_client::tl;

/// Value threaded from one action to the next. The two hand-offs are mutually
/// exclusive, so a single enum makes "a message and a URL at once" unrepresentable.
pub(super) enum Carry {
    /// Nothing to hand to the next action.
    None,
    /// The last message we sent; `click` searches it for inline buttons.
    /// Boxed because `Message` is large relative to the other variants.
    Message(Box<Message>),
    /// A URL opened by a `click` on a URL button; `browserless` consumes it as `variables.url`.
    Url(String),
}

impl Carry {
    /// The carried message, if the previous action left one (for `click`).
    fn into_message(self) -> Option<Message> {
        match self {
            Carry::Message(message) => Some(*message),
            _ => None,
        }
    }

    /// The carried URL, if the previous action left one (for `browserless`).
    fn into_url(self) -> Option<String> {
        match self {
            Carry::Url(url) => Some(url),
            _ => None,
        }
    }
}

pub(super) async fn execute_action(
    client: &Client,
    peer: PeerRef,
    action: &Action,
    carry: Carry,
) -> Result<Carry> {
    match action {
        Action::Text(text) => Ok(Carry::Message(Box::new(
            client.send_message(peer, text.text.as_str()).await?,
        ))),
        Action::Dice(dice) => {
            let message = InputMessage::new().media(tl::types::InputMediaDice {
                emoticon: dice.dice.clone(),
            });
            Ok(Carry::Message(Box::new(
                client.send_message(peer, message).await?,
            )))
        }
        Action::Click(click) => {
            let previous_message = carry.into_message();
            match click::click_inline_keyboard(client, peer, &click.key, previous_message.as_ref())
                .await?
            {
                Some(url) => Ok(Carry::Url(url)),
                None => Ok(Carry::None),
            }
        }
        Action::Llm(llm) => llm::execute_llm_action(client, peer, &llm.prompt, carry).await,
        Action::Browserless(browserless) => {
            let url = match carry.into_url() {
                Some(url) => Some(miniapp::resolve_url_if_miniapp(client, &url).await?),
                None => None,
            };
            browserless::execute_browserless_action(browserless, url).await?;
            Ok(Carry::None)
        }
    }
}
