use std::{
    env,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::Mutex;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const DEFAULT_ICON: &str = "https://www.redditstatic.com/shreddit/assets/favicon/192x192.png";

// ---------------------------------------------------------------- config

struct Config {
    /// App-only OAuth (client_credentials grant): no Reddit user is logged in.
    client_id: String,
    client_secret: String,
    api_base: String,
    auth_url: String,
    link_base: String,
    user_agent: String,
    icon: String,
    public_url: Option<String>,
    default_limit: String,
    port: u16,
}

fn env_opt(k: &str) -> Option<String> {
    env::var(k).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let (Some(client_id), Some(client_secret)) =
            (env_opt("REDDIT_CLIENT_ID"), env_opt("REDDIT_CLIENT_SECRET"))
        else {
            return Err("set REDDIT_CLIENT_ID and REDDIT_CLIENT_SECRET".into());
        };
        let trim = |s: String| s.trim_end_matches('/').to_string();
        Ok(Config {
            client_id,
            client_secret,
            api_base: trim(env_opt("REDDIT_API_BASE").unwrap_or_else(|| "https://oauth.reddit.com".into())),
            auth_url: env_opt("REDDIT_AUTH_URL")
                .unwrap_or_else(|| "https://www.reddit.com/api/v1/access_token".into()),
            link_base: trim(env_opt("LINK_BASE").unwrap_or_else(|| "https://www.reddit.com".into())),
            user_agent: env_opt("USER_AGENT")
                .unwrap_or_else(|| "linux:borkov:0.3.0 (self-hosted)".into()),
            icon: env_opt("FEED_ICON").unwrap_or_else(|| DEFAULT_ICON.into()),
            public_url: env_opt("PUBLIC_URL").map(trim),
            default_limit: env_opt("DEFAULT_LIMIT").unwrap_or_else(|| "25".into()),
            port: env_opt("PORT").and_then(|v| v.parse().ok()).unwrap_or(80),
        })
    }
}

// ---------------------------------------------------------------- state

struct Token {
    value: String,
    expires: Instant,
}

struct Inner {
    cfg: Config,
    http: reqwest::Client,
    token: Mutex<Option<Token>>,
}

type App = Arc<Inner>;

#[derive(Debug)]
enum Error {
    Upstream(StatusCode, String),
    /// Reddit said 429; pass its reset time on so the feed reader backs off.
    RateLimited(Option<String>),
    Internal(String),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            Error::Upstream(c, m) => (c, m),
            Error::RateLimited(retry_after) => {
                eprintln!("error 429: rate limited by reddit (retry after {retry_after:?}s)");
                let mut r = (StatusCode::TOO_MANY_REQUESTS, "rate limited by reddit\n").into_response();
                if let Some(v) = retry_after.and_then(|v| v.parse().ok()) {
                    r.headers_mut().insert(header::RETRY_AFTER, v);
                }
                return r;
            }
            Error::Internal(m) => (StatusCode::BAD_GATEWAY, m),
        };
        eprintln!("error {code}: {msg}");
        (code, format!("{msg}\n")).into_response()
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Internal(format!("upstream request failed: {e}"))
    }
}

// ---------------------------------------------------------------- reddit api

impl Inner {
    /// Current app-only token. `rejected` is a token Reddit just answered 401 to:
    /// it gets replaced, unless a concurrent request already replaced it.
    async fn token(&self, rejected: Option<&str>) -> Result<String, Error> {
        let mut slot = self.token.lock().await;
        if let Some(t) = slot.as_ref() {
            if Instant::now() < t.expires && rejected != Some(t.value.as_str()) {
                return Ok(t.value.clone());
            }
        }
        let resp = self
            .http
            .post(&self.cfg.auth_url)
            .basic_auth(&self.cfg.client_id, Some(&self.cfg.client_secret))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await?;
        let status = resp.status();
        let body: Value = serde_json::from_slice(&resp.bytes().await?)
            .map_err(|e| Error::Internal(format!("token response ({status}): {e}")))?;
        let Some(value) = body["access_token"].as_str().map(str::to_string) else {
            return Err(Error::Internal(format!("token request failed ({status}): {body}")));
        };
        // refresh a minute early
        let ttl = body["expires_in"].as_u64().unwrap_or(3600).saturating_sub(60);
        eprintln!("obtained reddit access token (valid ~{ttl}s)");
        *slot = Some(Token {
            value: value.clone(),
            expires: Instant::now() + Duration::from_secs(ttl),
        });
        Ok(value)
    }

    async fn api_get(&self, path_and_query: &str) -> Result<Value, Error> {
        let url = format!("{}{}", self.cfg.api_base, path_and_query);
        let mut rejected: Option<String> = None;
        loop {
            let tok = self.token(rejected.as_deref()).await?;
            let resp = self.http.get(&url).bearer_auth(&tok).send().await?;
            let status = resp.status();
            if status == StatusCode::UNAUTHORIZED && rejected.is_none() {
                rejected = Some(tok);
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS {
                let h = resp.headers();
                let reset = h
                    .get(header::RETRY_AFTER)
                    .or_else(|| h.get("x-ratelimit-reset"))
                    .and_then(|v| v.to_str().ok())
                    // reddit sometimes sends "123.0"; Retry-After wants whole seconds
                    .and_then(|v| v.trim().parse::<f64>().ok())
                    .map(|s| (s.ceil() as u64).max(1).to_string());
                return Err(Error::RateLimited(reset));
            }
            if status.is_redirection() {
                // reddit redirects unknown subreddits to search
                return Err(Error::Upstream(StatusCode::NOT_FOUND, "not found on reddit".into()));
            }
            if !status.is_success() {
                let code = match status.as_u16() {
                    403 | 404 => status,
                    _ => StatusCode::BAD_GATEWAY, // incl. 401: our credentials, not the client's
                };
                // keep reddit's explanation: www-authenticate (OAuth errors) + start of the body
                let auth = resp
                    .headers()
                    .get(header::WWW_AUTHENTICATE)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| format!(" [www-authenticate: {v}]"))
                    .unwrap_or_default();
                let body = resp.bytes().await.unwrap_or_default();
                let body = String::from_utf8_lossy(&body[..body.len().min(400)])
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                return Err(Error::Upstream(
                    code,
                    format!("reddit returned {status} for {url}{auth}: {body}"),
                ));
            }
            // parse straight from the body bytes: no intermediate String copy
            return serde_json::from_slice(&resp.bytes().await?)
                .map_err(|e| Error::Internal(format!("bad JSON from {url}: {e}")));
        }
    }

    /// Description of the subreddit / user the feed belongs to (the feed <subtitle>).
    async fn subtitle(&self, ctx: &Ctx) -> String {
        let (path, pointer) = match ctx {
            // multireddits and r/all, r/popular have no about page: don't waste a call
            Ctx::Sub(s)
                if !s.contains('+') && !["all", "popular"].contains(&s.to_ascii_lowercase().as_str()) =>
            {
                (format!("/r/{s}/about"), "/data/public_description")
            }
            Ctx::User(u) => (format!("/user/{u}/about"), "/data/subreddit/public_description"),
            _ => return String::new(),
        };
        match self.api_get(&format!("{path}?raw_json=1")).await {
            Ok(v) => v.pointer(pointer).and_then(Value::as_str).unwrap_or("").to_string(),
            Err(_) => String::new(),
        }
    }
}

// ---------------------------------------------------------------- path -> feed

/// What the feed is "about", derived from the path. Only used for the
/// feed-level <category>, <subtitle> and <title>; never to decide what to fetch.
enum Ctx {
    Sub(String),
    User(String),
    Other,
}

/// `/r/x/top.rss`, `/r/x/.rss`, `/u/x.rss`... -> normalized reddit path without `.rss`.
fn reddit_path(path: &str) -> Option<String> {
    let p = path.strip_suffix(".rss")?.trim_end_matches('/');
    let ok = p
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_-+./:".contains(c))
        && !p.split('/').any(|seg| seg == "..");
    if !ok {
        return None;
    }
    let p = match p.strip_prefix("/u/") {
        Some(rest) => format!("/user/{rest}"),
        None => p.to_string(),
    };
    Some(p)
}

fn segments(p: &str) -> Vec<&str> {
    p.split('/').filter(|s| !s.is_empty()).collect()
}

/// The API serves listings at explicit endpoints only for these two bare forms.
fn api_path(p: &str) -> String {
    match segments(p).as_slice() {
        [] => "/hot".into(),
        ["r", s] => format!("/r/{s}/hot"),
        ["user", u] => format!("/user/{u}/overview"),
        _ => p.into(),
    }
}

fn context(p: &str) -> Ctx {
    match segments(p).as_slice() {
        ["r", s, ..] => Ctx::Sub(s.to_string()),
        ["user", u, ..] => Ctx::User(u.to_string()),
        _ => Ctx::Other,
    }
}

/// Old-reddit feed titles. Anything not listed falls back to "<segment> : <sub>"
/// / "<segment> by <user>" / the path itself, so unknown endpoints still get a title.
fn feed_title(p: &str, thread_title: Option<&str>) -> String {
    fn label(sort: &str) -> &str {
        match sort {
            "new" => "newest submissions",
            "top" => "top scoring links",
            "controversial" => "most controversial links",
            "rising" => "rising threads",
            "comments" => "newest comments",
            other => other,
        }
    }
    match segments(p).as_slice() {
        [] | ["hot"] => "reddit: the front page of the internet".into(),
        [sort] if !["r", "user"].contains(sort) => label(sort).into(),
        ["r", s] | ["r", s, "hot"] => s.to_string(),
        ["r", s, "comments", _, ..] => thread_title.map(str::to_string).unwrap_or_else(|| s.to_string()),
        ["r", s, sort, ..] => format!("{} : {s}", label(sort)),
        ["user", u] | ["user", u, "overview"] => format!("overview for {u}"),
        ["user", u, what, ..] => format!("{what} by {u}"),
        segs => segs.join("/"),
    }
}

/// Flatten a listing, an array of listings (threads), and nested comment replies
/// into a list of `{kind, data}` things, in display order.
fn collect<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
    match v {
        Value::Array(a) => a.iter().for_each(|x| collect(x, out)),
        Value::Object(_) => match v.get("kind").and_then(Value::as_str) {
            Some("Listing") => {
                if let Some(children) = v.pointer("/data/children").and_then(Value::as_array) {
                    children.iter().for_each(|c| collect(c, out));
                }
            }
            Some("more") | None => {}
            Some(_) => {
                out.push(v);
                if let Some(r) = v.pointer("/data/replies") {
                    collect(r, out);
                }
            }
        },
        _ => {}
    }
}

fn is_listing(v: &Value) -> bool {
    match v {
        Value::Array(a) => !a.is_empty() && a.iter().all(is_listing),
        _ => v.get("kind").and_then(Value::as_str) == Some("Listing"),
    }
}

// ---------------------------------------------------------------- handler

async fn feed(State(app): State<App>, uri: Uri, headers: HeaderMap) -> Response {
    let Some(rpath) = reddit_path(uri.path()) else {
        return (
            StatusCode::NOT_FOUND,
            "usage: take any reddit listing URL and append .rss to its path, \
             e.g. /r/selfhosted/new.rss or /user/someone/comments.rss\n",
        )
            .into_response();
    };
    let path_q = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();

    // pass every query param through; force raw_json, default the limit
    // (scoped: the serializer isn't Send and must not live across an await)
    let api_url = {
        let mut q = url::form_urlencoded::Serializer::new(String::new());
        q.append_pair("raw_json", "1");
        let mut has_limit = false;
        for (k, v) in url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()) {
            match k.as_ref() {
                "raw_json" => {}
                "limit" => {
                    has_limit = true;
                    q.append_pair(&k, &v);
                }
                _ => {
                    q.append_pair(&k, &v);
                }
            }
        }
        if !has_limit {
            q.append_pair("limit", &app.cfg.default_limit);
        }
        format!("{}?{}", api_path(&rpath), q.finish())
    };

    // listing and feed description are independent, fetch both at once
    let ctx = context(&rpath);
    let (json, subtitle) = tokio::join!(app.api_get(&api_url), app.subtitle(&ctx));
    let json = match json {
        Ok(j) => j,
        Err(e) => return e.into_response(),
    };
    if !is_listing(&json) {
        return Error::Upstream(StatusCode::NOT_FOUND, format!("{rpath} is not a listing")).into_response();
    }
    let mut items = Vec::new();
    collect(&json, &mut items);

    // a thread is [post listing, comments listing]; comments there lack link_title
    let thread_title = json
        .pointer("/0/data/children/0/data/title")
        .and_then(Value::as_str);

    let self_base = app.cfg.public_url.clone().unwrap_or_else(|| {
        let host = headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost");
        format!("http://{host}")
    });
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let alt_path = if rpath.is_empty() { "/" } else { &rpath };

    let r = Renderer { base: &app.cfg.link_base, thread_title };
    let body = r.feed(&FeedMeta {
        ctx: &ctx,
        title: &feed_title(&rpath, thread_title),
        subtitle: &subtitle,
        icon: &app.cfg.icon,
        id: &path_q,
        self_link: &format!("{self_base}{path_q}"),
        alternate: &format!("{}{alt_path}{query}", app.cfg.link_base),
        items: &items,
    });
    eprintln!("served {path_q} ({} entries)", items.len());
    (
        [
            (header::CONTENT_TYPE, "application/atom+xml; charset=UTF-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------- rendering

struct FeedMeta<'a> {
    ctx: &'a Ctx,
    title: &'a str,
    subtitle: &'a str,
    icon: &'a str,
    id: &'a str,
    self_link: &'a str,
    alternate: &'a str,
    items: &'a [&'a Value],
}

/// XML text/attribute escaping (matches Reddit's: apostrophes left alone).
/// Also drops characters XML 1.0 forbids (control chars, U+FFFE/U+FFFF): one
/// stray \u0008 in a title would otherwise make the whole feed unparseable.
fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\t' | '\n' | '\r' => o.push(c),
            '\u{0}'..='\u{1F}' | '\u{FFFE}' | '\u{FFFF}' => {}
            _ => o.push(c),
        }
    }
    o
}

fn ts(secs: f64) -> String {
    DateTime::<Utc>::from_timestamp(secs as i64, 0)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%S+00:00")
        .to_string()
}

/// Collapse newlines to single spaces like the old feeds did, but keep them inside <pre>.
fn collapse_newlines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_pre = false;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if c == '<' {
            let rest = &s[i..];
            if rest.starts_with("<pre") {
                in_pre = true;
            } else if rest.starts_with("</pre>") {
                in_pre = false;
            }
        }
        if c == '\n' && !in_pre {
            while matches!(it.peek(), Some((_, '\n'))) {
                it.next();
            }
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// String field, empty string if missing/null.
fn s<'a>(d: &'a Value, k: &str) -> &'a str {
    d.get(k).and_then(Value::as_str).unwrap_or("")
}

/// First non-empty string among several fields.
fn first<'a>(d: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter().map(|k| s(d, k)).find(|v| !v.is_empty()).unwrap_or("")
}

struct Renderer<'a> {
    base: &'a str,
    thread_title: Option<&'a str>,
}

impl Renderer<'_> {
    fn abs(&self, u: &str) -> String {
        if u.starts_with('/') {
            format!("{}{u}", self.base)
        } else {
            u.to_string()
        }
    }

    fn author(&self, d: &Value) -> String {
        let a = esc(s(d, "author"));
        format!(
            "<author><name>/u/{a}</name><uri>{}/user/{a}</uri></author>",
            esc(self.base)
        )
    }

    fn feed(&self, m: &FeedMeta) -> String {
        let mut x = String::with_capacity(64 * 1024);
        x.push_str(r#"<?xml version="1.0" encoding="UTF-8"?><feed xmlns="http://www.w3.org/2005/Atom" xmlns:media="http://search.yahoo.com/mrss/">"#);
        match m.ctx {
            Ctx::Sub(s) => x += &format!(r#"<category term="{0}" label="r/{0}"/>"#, esc(s)),
            Ctx::User(u) => x += &format!(r#"<category term="u_{0}" label="u/{0}"/>"#, esc(u)),
            Ctx::Other => {}
        }
        x += &format!(
            r#"<updated>{u}</updated><icon>{i}</icon><id>{id}</id><link rel="self" href="{sl}" type="application/atom+xml" /><link rel="alternate" href="{al}" type="text/html" /><subtitle>{sub}</subtitle><title>{t}</title>"#,
            u = Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00"),
            i = esc(m.icon),
            id = esc(m.id),
            sl = esc(m.self_link),
            al = esc(m.alternate),
            sub = esc(m.subtitle),
            t = esc(m.title),
        );
        for item in m.items {
            let d = &item["data"];
            match item.get("kind").and_then(Value::as_str) {
                Some("t3") => self.post(&mut x, d),
                Some("t1") => self.comment(&mut x, d),
                _ => self.generic(&mut x, d),
            }
        }
        x.push_str("</feed>");
        x
    }

    fn thumb<'v>(d: &'v Value) -> Option<&'v str> {
        Some(s(d, "thumbnail")).filter(|t| t.starts_with("http"))
    }

    /// t3 — a submission.
    fn post(&self, x: &mut String, d: &Value) {
        let permalink = self.abs(s(d, "permalink"));
        let link = match s(d, "url") {
            "" => permalink.clone(),
            u => self.abs(u),
        };
        let author = s(d, "author");
        let title = s(d, "title");
        let body = collapse_newlines(s(d, "selftext_html").trim());
        let footer = format!(
            "&#32; submitted by &#32; <a href=\"{}\"> /u/{} </a> <br/> \
             <span><a href=\"{}\">[link]</a></span> &#32; \
             <span><a href=\"{}\">[comments]</a></span>",
            esc(&format!("{}/user/{author}", self.base)),
            esc(author),
            esc(&link),
            esc(&permalink)
        );
        let main = if body.is_empty() { footer } else { format!("{body} {footer}") };
        let html = match Self::thumb(d) {
            Some(th) => format!(
                "<table> <tr><td> <a href=\"{pl}\"> <img src=\"{th}\" alt=\"{t}\" title=\"{t}\" /> </a> </td><td> {main} </td></tr></table>",
                pl = esc(&permalink),
                th = esc(th),
                t = esc(title),
            ),
            None => main,
        };
        let when = ts(d["created_utc"].as_f64().unwrap_or(0.0));
        *x += "<entry>";
        *x += &self.author(d);
        *x += &format!(
            r#"<category term="{s}" label="r/{s}"/><content type="html">{c}</content><id>{id}</id>"#,
            s = esc(s(d, "subreddit")),
            c = esc(&html),
            id = esc(s(d, "name")),
        );
        if let Some(th) = Self::thumb(d) {
            *x += &format!(r#"<media:thumbnail url="{}" />"#, esc(th));
        }
        *x += &format!(
            r#"<link href="{l}" /><updated>{w}</updated><published>{w}</published><title>{t}</title></entry>"#,
            l = esc(&permalink),
            w = when,
            t = esc(title),
        );
    }

    /// t1 — a comment.
    fn comment(&self, x: &mut String, d: &Value) {
        let link_title = match s(d, "link_title") {
            "" => self.thread_title.unwrap_or(""),
            t => t,
        };
        let permalink = self.abs(s(d, "permalink"));
        let body = collapse_newlines(s(d, "body_html").trim());
        *x += "<entry>";
        *x += &self.author(d);
        *x += &format!(
            r#"<category term="{s}" label="r/{s}" /><content type="html">{c}</content><id>{id}</id><link href="{l}"/><updated>{w}</updated><title>/u/{a} on {t}</title></entry>"#,
            s = esc(s(d, "subreddit")),
            c = esc(&body),
            id = esc(s(d, "name")),
            l = esc(&permalink),
            w = ts(d["created_utc"].as_f64().unwrap_or(0.0)),
            a = esc(s(d, "author")),
            t = esc(link_title),
        );
    }

    /// Anything else (subreddit lists, users, ...): best effort from common fields.
    fn generic(&self, x: &mut String, d: &Value) {
        *x += "<entry>";
        if !s(d, "author").is_empty() {
            *x += &self.author(d);
        }
        let html = collapse_newlines(
            first(d, &["public_description_html", "description_html", "body_html", "selftext_html"]).trim(),
        );
        *x += &format!(r#"<content type="html">{}</content><id>{}</id>"#, esc(&html), esc(s(d, "name")));
        let link = first(d, &["permalink", "url"]);
        if !link.is_empty() {
            *x += &format!(r#"<link href="{}" />"#, esc(&self.abs(link)));
        }
        if let Some(t) = d["created_utc"].as_f64() {
            *x += &format!("<updated>{}</updated>", ts(t));
        }
        *x += &format!(
            "<title>{}</title></entry>",
            esc(first(d, &["title", "display_name_prefixed", "name"]))
        );
    }
}

// ---------------------------------------------------------------- main

// One thread is plenty: the work is waiting on Reddit, not CPU. Fewer threads = less memory.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(1);
        }
    };
    let port = cfg.port;
    let mut builder = reqwest::Client::builder()
        .user_agent(cfg.user_agent.clone())
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20));
    // HTTP/2 only towards Reddit: its CDN bot filter rejected our HTTP/1.1 requests (403)
    // while accepting identical HTTP/2 ones. Plain-http bases (local test mocks) stay HTTP/1.1.
    if cfg.api_base.starts_with("https://") && cfg.auth_url.starts_with("https://") {
        builder = builder.http2_prior_knowledge();
    }
    let http = builder.build().expect("http client");
    let app: App = Arc::new(Inner {
        cfg,
        http,
        token: Mutex::new(None),
    });

    let router = Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .fallback(feed)
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| panic!("bind :{port}: {e}"));
    eprintln!("listening on :{port}");
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            // PID 1 in a container gets SIGTERM from `docker stop`
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("sigterm handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = term.recv() => {},
            }
        })
        .await
        .unwrap();
}
