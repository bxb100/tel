use anyhow::{Context, Result, anyhow};
use grammers_client::{Client, tl};
use regex_lite::Regex;
use std::sync::LazyLock;

struct MiniappLink {
    bot: String,
    short_name: String,
    start_param: Option<String>,
}

static MINIAPP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https?://t\.me/(?P<bot>[^/?#]+)/(?P<app>[^/?#]+)(?:[?#]|$)").unwrap()
});
static STARTAPP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[?&]startapp=(?P<v>[^&#]*)").unwrap());

fn parse_miniapp_link(url: &str) -> Option<MiniappLink> {
    let caps = MINIAPP_RE.captures(url.trim())?;
    let short_name = &caps["app"];
    if short_name.bytes().all(|b| b.is_ascii_digit()) {
        return None; // t.me/频道/123 是帖子链接，不是 miniapp
    }
    let start_param = STARTAPP_RE
        .captures(url)
        .map(|c| c["v"].to_string())
        .filter(|v| !v.is_empty());
    Some(MiniappLink {
        bot: caps["bot"].to_string(),
        short_name: short_name.to_string(),
        start_param,
    })
}

pub(super) async fn resolve_url_if_miniapp(client: &Client, url: &str) -> Result<String> {
    let Some(link) = parse_miniapp_link(url) else {
        return Ok(url.to_string()); // 不是 miniapp 链接，原样返回
    };

    let peer = client
        .resolve_username(&link.bot)
        .await
        .with_context(|| format!("failed to resolve miniapp bot @{}", link.bot))?
        .with_context(|| format!("miniapp bot @{} not found", link.bot))?;
    let bot_ref = peer
        .to_ref()
        .await
        .map_err(|err| anyhow!("{}", err))?
        .with_context(|| format!("cannot use @{} as a peer", link.bot))?;

    let result = client
        .invoke(&tl::functions::messages::RequestAppWebView {
            write_allowed: false,
            compact: false,
            fullscreen: false,
            peer: (&bot_ref).into(),
            app: tl::enums::InputBotApp::ShortName(tl::types::InputBotAppShortName {
                bot_id: (&bot_ref).into(),
                short_name: link.short_name.clone(),
            }),
            start_param: link.start_param,
            theme_params: None,
            platform: "android".to_string(),
        })
        .await
        .with_context(|| {
            format!(
                "failed to resolve miniapp url @{}/{}",
                link.bot, link.short_name
            )
        })?;

    match result {
        tl::enums::WebViewResult::Url(url) => Ok(url.url),
    }
}
