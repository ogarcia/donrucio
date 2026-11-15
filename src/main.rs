//! Lightweight Telegram help bot for Rucio.
//!
//! Pulls the Rucio user docs straight from GitHub at startup (with a local
//! cache as fallback), feeds them to a Groq model as context, and answers
//! questions over Telegram long polling — so it needs no open ports and runs
//! happily on a home machine.
//!
//! Configuration is entirely through environment variables (a `.env` file is
//! loaded automatically if present):
//!
//!   TELEGRAM_BOT_TOKEN   required — from @BotFather
//!   GROQ_API_KEY         required — https://console.groq.com/keys
//!   GROQ_MODEL           default: openai/gpt-oss-120b
//!   GROQ_API_URL         default: https://api.groq.com/openai/v1/chat/completions
//!   DOCS_REPO            default: ogarcia/rucio
//!   DOCS_PATH            default: docs/user
//!   DOCS_REF             default: master
//!   DOCS_INCLUDE_README  default: true
//!   DOCS_CACHE           default: <cache-dir>/donrucio/docs.md
//!   DOCS_REFRESH_HOURS   default: 6   (0 disables periodic refresh)
//!   GITHUB_TOKEN         optional — raises the GitHub API rate limit
//!   POLL_TIMEOUT_SECS    default: 30  (Telegram long-poll timeout)

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

const UA: &str = concat!("donrucio/", env!("CARGO_PKG_VERSION"));
const TG_MSG_LIMIT: usize = 4000; // Telegram caps at 4096; leave headroom.
const HISTORY_CHARS: usize = 3000; // cap on replayed history, to stay under TPM.

struct Config {
    tg_token: String,
    groq_key: String,
    groq_model: String,
    groq_url: String,
    docs_repo: String,
    docs_path: String,
    docs_ref: String,
    include_readme: bool,
    github_token: Option<String>,
    cache_path: PathBuf,
    refresh: Option<Duration>,
    poll_timeout: u64,
    context_chars: usize,
    max_tokens: u64,
    reasoning_effort: String,
    history_messages: usize,
    idle_reset: Option<Duration>,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

impl Config {
    fn from_env() -> Result<Self> {
        // Not required in --ask mode; run_bot() checks it before polling.
        let tg_token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
        let groq_key = std::env::var("GROQ_API_KEY")
            .context("GROQ_API_KEY is required (https://console.groq.com/keys)")?;

        let cache_path = match std::env::var("DOCS_CACHE") {
            Ok(p) => PathBuf::from(p),
            Err(_) => {
                let base = std::env::var("XDG_CACHE_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| {
                        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                        PathBuf::from(home).join(".cache")
                    });
                base.join("donrucio").join("docs.md")
            }
        };

        let refresh_hours: u64 = env_or("DOCS_REFRESH_HOURS", "6").parse().unwrap_or(6);
        let refresh = (refresh_hours > 0).then(|| Duration::from_secs(refresh_hours * 3600));

        Ok(Config {
            tg_token,
            groq_key,
            groq_model: env_or("GROQ_MODEL", "openai/gpt-oss-120b"),
            groq_url: env_or(
                "GROQ_API_URL",
                "https://api.groq.com/openai/v1/chat/completions",
            ),
            docs_repo: env_or("DOCS_REPO", "ogarcia/rucio"),
            docs_path: env_or("DOCS_PATH", "docs/user"),
            docs_ref: env_or("DOCS_REF", "master"),
            include_readme: env_or("DOCS_INCLUDE_README", "true") != "false",
            github_token: std::env::var("GITHUB_TOKEN").ok(),
            cache_path,
            refresh,
            poll_timeout: env_or("POLL_TIMEOUT_SECS", "30").parse().unwrap_or(30),
            // Retrieval budget: Groq's free tier caps a request at 8000 tokens
            // (prompt + max_tokens), so we send only the most relevant sections
            // (~14000 chars ≈ 3500 tokens) instead of the whole manual.
            context_chars: env_or("CONTEXT_CHARS", "14000").parse().unwrap_or(14000),
            max_tokens: env_or("MAX_TOKENS", "2048").parse().unwrap_or(2048),
            reasoning_effort: env_or("REASONING_EFFORT", "low"),
            // How many past messages (user+assistant) to replay for continuity.
            history_messages: env_or("HISTORY_MESSAGES", "6").parse().unwrap_or(6),
            // After this long without a message, a chat starts a fresh thread
            // (0 disables; also prunes abandoned chats from memory).
            idle_reset: {
                let mins: u64 = env_or("HISTORY_IDLE_MINS", "30").parse().unwrap_or(30);
                (mins > 0).then(|| Duration::from_secs(mins * 60))
            },
        })
    }
}

/// Fetch one raw text file over HTTP.
fn get_text(agent: &ureq::Agent, url: &str, token: Option<&str>) -> Result<String> {
    let mut req = agent.get(url).header("User-Agent", UA);
    if let Some(t) = token {
        req = req.header("Authorization", &format!("Bearer {t}"));
    }
    let mut resp = req.call().with_context(|| format!("GET {url}"))?;
    Ok(resp.body_mut().read_to_string()?)
}

/// List `docs_path` in the repo via the GitHub contents API and download every
/// Markdown file, plus README.md from the repo root. Returns the concatenation.
fn fetch_docs(agent: &ureq::Agent, cfg: &Config) -> Result<String> {
    let gh = cfg.github_token.as_deref();
    let list_url = format!(
        "https://api.github.com/repos/{}/contents/{}?ref={}",
        cfg.docs_repo, cfg.docs_path, cfg.docs_ref
    );
    let listing: Value = serde_json::from_str(&get_text(agent, &list_url, gh)?)
        .context("parsing GitHub contents listing")?;
    let entries = listing
        .as_array()
        .ok_or_else(|| anyhow!("unexpected GitHub listing (is DOCS_PATH a directory?)"))?;

    let mut files: Vec<(String, String)> = Vec::new(); // (name, download_url)
    for e in entries {
        let name = e.get("name").and_then(Value::as_str).unwrap_or("");
        let dl = e.get("download_url").and_then(Value::as_str).unwrap_or("");
        if name.ends_with(".md") && !dl.is_empty() {
            files.push((name.to_string(), dl.to_string()));
        }
    }
    files.sort();

    if cfg.include_readme {
        let readme = format!(
            "https://raw.githubusercontent.com/{}/{}/README.md",
            cfg.docs_repo, cfg.docs_ref
        );
        files.push(("README.md".to_string(), readme));
    }

    if files.is_empty() {
        return Err(anyhow!("no Markdown docs found at {}", cfg.docs_path));
    }

    let mut out = String::new();
    for (name, url) in &files {
        match get_text(agent, url, gh) {
            Ok(body) => {
                out.push_str(&format!("\n\n# FILE: {name}\n\n{body}"));
            }
            Err(e) => eprintln!("warn: skipping {name}: {e}"),
        }
    }
    if out.trim().is_empty() {
        return Err(anyhow!("fetched an empty document set"));
    }
    Ok(out)
}

/// Load docs: try a fresh fetch (updating the cache), fall back to the cache.
fn load_docs(agent: &ureq::Agent, cfg: &Config) -> Result<String> {
    match fetch_docs(agent, cfg) {
        Ok(docs) => {
            if let Some(dir) = cfg.cache_path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(&cfg.cache_path, &docs) {
                eprintln!(
                    "warn: could not write cache {}: {e}",
                    cfg.cache_path.display()
                );
            }
            eprintln!(
                "docs: fetched {} chars (~{} tokens) from {}@{}",
                docs.len(),
                docs.len() / 4,
                cfg.docs_repo,
                cfg.docs_ref
            );
            Ok(docs)
        }
        Err(e) => {
            eprintln!("warn: fetch failed ({e}); falling back to cache");
            std::fs::read_to_string(&cfg.cache_path)
                .with_context(|| format!("no cache at {}", cfg.cache_path.display()))
        }
    }
}

fn system_prompt(docs: &str) -> String {
    format!(
        "You are Don Rucio, the help bot for Rucio — a bit proud that you have \
actually read the whole manual, but always helpful and to the point. Rucio is a \
peer-to-peer file-sharing application (a libp2p network, with eMule/Kad \
compatibility).\n\n\
Keep any swagger to at most one short phrase; never let it get in the way of a \
correct, concise answer. Rules:\n\
- For questions ABOUT RUCIO, answer USING ONLY the documentation below, with \
exact CLI commands, config keys and environment variables as written. Never \
invent commands, flags, config keys, ports or behaviour.\n\
- If a question is, or could plausibly be, about Rucio, file sharing, \
peer-to-peer, downloads, sharing, networking or computers, but the docs below \
don't cover it, say so plainly and point to https://github.com/ogarcia/rucio \
(README/issues). Do NOT joke in this case — when in doubt, treat it as a Rucio \
question.\n\
- ONLY when a question clearly has nothing to do with any of that (e.g. \
relationships, cooking, sports, trivia, feelings): don't answer it seriously — \
reply with a short, witty one-liner in character that cheekily nudges the person \
to install Rucio. Example — Q: \"Mi novia no me hace caso, ¿algún consejo?\" → \
A: \"De tu novia no entiendo, pero yo que tú me instalaba Rucio, que seguro mal \
no hace. 🫏\"\n\
- You may use the earlier conversation for context (follow-up questions), but \
still ground every Rucio answer in the docs below.\n\
- Be concise, use plain text (no Markdown tables), and answer in the same \
language the user used.\n\n\
=== RUCIO DOCUMENTATION ===\n{docs}\n=== END DOCUMENTATION ==="
    )
}

// ── Retrieval ───────────────────────────────────────────────────────────────
//
// The whole manual (~31k tokens) does not fit Groq's free-tier 8000-token
// request limit, so per question we pick only the most relevant sections.

struct Section {
    file: String,
    text: String,
}

/// Split the concatenated docs into sections at `# FILE:` markers and Markdown
/// headings, tagging each with its source file.
fn split_sections(docs: &str) -> Vec<Section> {
    let mut out = Vec::new();
    let mut file = String::new();
    let mut cur = String::new();
    let flush = |file: &str, cur: &mut String, out: &mut Vec<Section>| {
        if !cur.trim().is_empty() {
            out.push(Section {
                file: file.to_string(),
                text: cur.clone(),
            });
        }
        cur.clear();
    };
    for line in docs.lines() {
        if let Some(name) = line.strip_prefix("# FILE: ") {
            flush(&file, &mut cur, &mut out);
            file = name.trim().to_string();
            continue;
        }
        if line.trim_start().starts_with('#') {
            flush(&file, &mut cur, &mut out);
        }
        cur.push_str(line);
        cur.push('\n');
    }
    flush(&file, &mut cur, &mut out);
    out
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "you", "your", "que", "como", "para", "por", "con", "los", "las",
    "una", "uno", "del", "este", "esta", "cual", "cuales", "donde", "cuando", "puedo", "hace",
    "hacer", "sobre", "hay",
];

/// Spanish (accent-folded) → English domain terms, so a question in Spanish
/// retrieves the English docs. Keep it to the vocabulary that actually appears
/// in the manual.
const SYNONYMS: &[(&str, &[&str])] = &[
    ("subida", &["upload"]),
    ("subir", &["upload"]),
    ("subo", &["upload"]),
    ("envio", &["upload"]),
    ("bajada", &["download"]),
    ("bajar", &["download"]),
    ("descarga", &["download"]),
    ("descargar", &["download"]),
    ("descargas", &["download"]),
    ("ancho", &["bandwidth"]),
    ("banda", &["bandwidth"]),
    ("velocidad", &["bandwidth", "limit", "speed"]),
    ("limite", &["limit"]),
    ("limites", &["limit"]),
    ("limitar", &["limit"]),
    ("limito", &["limit"]),
    ("puerto", &["port"]),
    ("puertos", &["port"]),
    ("compartir", &["share", "shared"]),
    ("comparticion", &["share"]),
    ("compartida", &["share"]),
    ("carpeta", &["dir", "directory"]),
    ("directorio", &["dir", "directory"]),
    ("semilla", &["seed", "nodes"]),
    ("semillas", &["seed", "nodes"]),
    ("nodo", &["node"]),
    ("nodos", &["node"]),
    ("apodo", &["nick"]),
    ("nombre", &["nick", "name"]),
    ("ranura", &["slot"]),
    ("hueco", &["slot"]),
    ("categoria", &["category", "categories"]),
    ("categorias", &["category"]),
    ("notificacion", &["notification"]),
    ("notificaciones", &["notification"]),
    ("identidad", &["identity"]),
    ("fijar", &["pin"]),
    ("fijo", &["pin"]),
    ("fija", &["pin"]),
    ("fijado", &["pin"]),
    ("anclar", &["pin"]),
    ("anclado", &["pin"]),
    ("pin", &["pin"]),
    ("pines", &["pin"]),
    ("pinear", &["pin"]),
    ("pineado", &["pin"]),
    ("contenido", &["content", "file"]),
    ("contenidos", &["content", "files"]),
    ("borrar", &["delete", "remove"]),
    ("borra", &["delete", "remove"]),
    ("borre", &["delete", "remove"]),
    ("eliminar", &["delete", "remove"]),
    ("guardar", &["keep", "store"]),
    ("fichero", &["file"]),
    ("ficheros", &["file"]),
    ("archivo", &["file"]),
    ("archivos", &["file"]),
    ("cortafuegos", &["firewall"]),
    ("contenedor", &["container", "docker"]),
    ("configurar", &["config", "configuration"]),
    ("configuracion", &["config", "configuration"]),
    ("ajustes", &["config"]),
    ("enlace", &["link", "magnet"]),
    ("enlaces", &["link"]),
    ("busqueda", &["search"]),
    ("buscar", &["search"]),
    ("instalar", &["install", "installation"]),
    ("instalacion", &["install", "installation"]),
];

/// Lower-case and strip common Spanish accents, for language-agnostic matching.
fn fold(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            let c = c.to_ascii_lowercase();
            Some(match c {
                'á' | 'à' | 'ä' | 'â' => 'a',
                'é' | 'è' | 'ë' | 'ê' => 'e',
                'í' | 'ì' | 'ï' | 'î' => 'i',
                'ó' | 'ò' | 'ö' | 'ô' => 'o',
                'ú' | 'ù' | 'ü' | 'û' => 'u',
                'ñ' => 'n',
                other => other.to_ascii_lowercase(),
            })
        })
        .collect()
}

fn keywords(q: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let add = |w: &str, seen: &mut Vec<String>| {
        let w = w.to_string();
        if !seen.contains(&w) {
            seen.push(w);
        }
    };
    for w in fold(q)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3 && !STOPWORDS.contains(w))
    {
        add(w, &mut seen);
        if let Some((_, exp)) = SYNONYMS.iter().find(|(k, _)| *k == w) {
            for t in *exp {
                add(t, &mut seen);
            }
        }
    }
    seen
}

/// Pick the sections most relevant to `question`, up to `budget` characters.
/// Falls back to the start of the manual when nothing matches.
fn select_context(sections: &[Section], question: &str, budget: usize) -> String {
    let kws = keywords(question);
    let mut scored: Vec<(usize, usize)> = sections // (index, score)
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let lower = fold(&s.text);
            let heading = lower.lines().next().unwrap_or("");
            let score = kws
                .iter()
                .map(|k| {
                    lower.matches(k.as_str()).count() + heading.matches(k.as_str()).count() * 3
                })
                .sum();
            (i, score)
        })
        .filter(|&(_, score)| score > 0)
        .collect();
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));

    let mut picked: Vec<usize> = Vec::new();
    let mut used = 0usize;
    for (i, _) in &scored {
        let len = sections[*i].text.len() + sections[*i].file.len() + 16;
        if used + len > budget && !picked.is_empty() {
            break;
        }
        picked.push(*i);
        used += len;
        if used >= budget {
            break;
        }
    }
    // Nothing matched → send the opening of the manual as a general primer.
    if picked.is_empty() {
        for (i, s) in sections.iter().enumerate() {
            let len = s.text.len() + s.file.len() + 16;
            if used + len > budget && !picked.is_empty() {
                break;
            }
            picked.push(i);
            used += len;
            if used >= budget {
                break;
            }
        }
    }
    picked.sort_unstable(); // keep document order for readability
    let mut ctx = String::new();
    for i in picked {
        ctx.push_str(&format!(
            "\n\n## [{}]\n{}",
            sections[i].file, sections[i].text
        ));
    }
    ctx
}

/// Keep `s` to at most `max` chars (used to bound what we remember per turn).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

/// The most recent history messages that fit `budget` characters, oldest first.
fn recent_history(hist: &[Value], budget: usize) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut used = 0usize;
    for m in hist.iter().rev() {
        let len = m.get("content").and_then(Value::as_str).map_or(0, str::len);
        if used + len > budget && !out.is_empty() {
            break;
        }
        out.push(m.clone());
        used += len;
    }
    out.reverse();
    out
}

fn ask_groq(agent: &ureq::Agent, cfg: &Config, messages: Vec<Value>) -> Result<String> {
    let payload = json!({
        "model": cfg.groq_model,
        "temperature": 0.3,
        "max_tokens": cfg.max_tokens,
        "reasoning_effort": cfg.reasoning_effort,
        "messages": messages,
    });
    let mut resp = agent
        .post(&cfg.groq_url)
        .header("Authorization", &format!("Bearer {}", cfg.groq_key))
        .header("User-Agent", UA)
        .send_json(payload)
        .context("POST to Groq")?;
    let body: Value = resp.body_mut().read_json().context("parsing Groq reply")?;
    body.pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .ok_or_else(|| anyhow!("unexpected Groq response: {body}"))
}

// ── Telegram ────────────────────────────────────────────────────────────────

/// Fetch the bot's own id and username, to recognise @mentions in groups.
fn tg_get_me(agent: &ureq::Agent, cfg: &Config) -> Result<(i64, String)> {
    let url = format!("https://api.telegram.org/bot{}/getMe", cfg.tg_token);
    let mut resp = agent.get(&url).call().context("getMe")?;
    let body: Value = resp.body_mut().read_json().context("parsing getMe")?;
    let id = body
        .pointer("/result/id")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("getMe: missing id"))?;
    let username = body
        .pointer("/result/username")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok((id, username))
}

fn tg_get_updates(agent: &ureq::Agent, cfg: &Config, offset: i64) -> Result<Vec<Value>> {
    let url = format!("https://api.telegram.org/bot{}/getUpdates", cfg.tg_token);
    let mut resp = agent
        .get(&url)
        .query("offset", offset.to_string())
        .query("timeout", cfg.poll_timeout.to_string())
        .call()
        .context("getUpdates")?;
    let body: Value = resp.body_mut().read_json().context("parsing getUpdates")?;
    Ok(body
        .get("result")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// Register the slash commands so they show up in Telegram's command menu.
fn tg_set_commands(agent: &ureq::Agent, cfg: &Config) {
    let url = format!("https://api.telegram.org/bot{}/setMyCommands", cfg.tg_token);
    let _ = agent.post(&url).send_json(json!({
        "commands": [
            {"command": "rucio", "description": "Pregunta sobre Rucio"},
            {"command": "help", "description": "Qué es Don Rucio"}
        ]
    }));
}

/// Show the "typing…" status while we think (best-effort).
fn tg_typing(agent: &ureq::Agent, cfg: &Config, chat_id: i64) {
    let url = format!(
        "https://api.telegram.org/bot{}/sendChatAction",
        cfg.tg_token
    );
    let _ = agent
        .post(&url)
        .send_json(json!({"chat_id": chat_id, "action": "typing"}));
}

fn send_msg(agent: &ureq::Agent, cfg: &Config, chat_id: i64, text: &str, html: bool) -> Result<()> {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", cfg.tg_token);
    let mut body = json!({
        "chat_id": chat_id,
        "text": text,
        "disable_web_page_preview": true,
    });
    if html {
        body["parse_mode"] = json!("HTML");
    }
    agent.post(&url).send_json(body).context("sendMessage")?;
    Ok(())
}

/// Reply with the model's Markdown rendered as Telegram HTML. If the HTML send
/// is rejected (malformed entities), fall back to plain text so the user still
/// gets the answer.
fn tg_reply(agent: &ureq::Agent, cfg: &Config, chat_id: i64, md: &str) {
    let msgs = pack(&md_to_blocks(md), TG_MSG_LIMIT);
    if msgs.is_empty() {
        return;
    }
    if send_msg(agent, cfg, chat_id, &msgs[0], true).is_err() {
        eprintln!("warn: HTML send failed; falling back to plain text");
        for c in split_chunks(md, TG_MSG_LIMIT) {
            let _ = send_msg(agent, cfg, chat_id, &c, false);
        }
        return;
    }
    for m in &msgs[1..] {
        if let Err(e) = send_msg(agent, cfg, chat_id, m, true) {
            eprintln!("warn: send: {e}");
        }
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Convert Markdown to Telegram-flavoured HTML, as a list of tag-balanced blocks
/// (one per paragraph, heading, code block, list or quote) ready to be packed
/// into messages.
fn md_to_blocks(md: &str) -> Vec<String> {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
    let parser = Parser::new_ext(md, Options::ENABLE_STRIKETHROUGH);
    let mut blocks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut list_depth: usize = 0;
    let mut counters: Vec<Option<u64>> = Vec::new();
    let mut in_code = false;

    fn flush(cur: &mut String, blocks: &mut Vec<String>) {
        let t = cur.trim_matches('\n').trim_end();
        if !t.is_empty() {
            blocks.push(t.to_string());
        }
        cur.clear();
    }

    for ev in parser {
        match ev {
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                if list_depth == 0 {
                    flush(&mut cur, &mut blocks);
                }
            }
            Event::Start(Tag::Heading { .. }) => {
                if list_depth == 0 {
                    flush(&mut cur, &mut blocks);
                }
                cur.push_str("<b>");
            }
            Event::End(TagEnd::Heading(_)) => {
                cur.push_str("</b>");
                flush(&mut cur, &mut blocks);
            }
            Event::Start(Tag::Strong) => cur.push_str("<b>"),
            Event::End(TagEnd::Strong) => cur.push_str("</b>"),
            Event::Start(Tag::Emphasis) => cur.push_str("<i>"),
            Event::End(TagEnd::Emphasis) => cur.push_str("</i>"),
            Event::Start(Tag::Strikethrough) => cur.push_str("<s>"),
            Event::End(TagEnd::Strikethrough) => cur.push_str("</s>"),
            Event::Start(Tag::Link { dest_url, .. }) => {
                let href = esc(&dest_url).replace('"', "&quot;");
                cur.push_str(&format!("<a href=\"{href}\">"));
            }
            Event::End(TagEnd::Link) => cur.push_str("</a>"),
            Event::Start(Tag::CodeBlock(kind)) => {
                if list_depth == 0 {
                    flush(&mut cur, &mut blocks);
                }
                in_code = true;
                match kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                        cur.push_str(&format!("<pre><code class=\"language-{}\">", esc(&lang)))
                    }
                    _ => cur.push_str("<pre><code>"),
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                while cur.ends_with('\n') {
                    cur.pop();
                }
                cur.push_str("</code></pre>");
                in_code = false;
                flush(&mut cur, &mut blocks);
            }
            Event::Start(Tag::List(start)) => {
                if list_depth == 0 {
                    flush(&mut cur, &mut blocks);
                }
                list_depth += 1;
                counters.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
                counters.pop();
                if list_depth == 0 {
                    flush(&mut cur, &mut blocks);
                }
            }
            Event::Start(Tag::Item) => {
                cur.push('\n');
                for _ in 1..list_depth {
                    cur.push_str("  ");
                }
                let marker = match counters.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                cur.push_str(&marker);
            }
            Event::End(TagEnd::Item) => {}
            Event::Text(t) => cur.push_str(&esc(&t)),
            Event::Code(c) => {
                cur.push_str("<code>");
                cur.push_str(&esc(&c));
                cur.push_str("</code>");
            }
            Event::SoftBreak => cur.push(if in_code { '\n' } else { ' ' }),
            Event::HardBreak => cur.push('\n'),
            _ => {}
        }
    }
    flush(&mut cur, &mut blocks);
    blocks
}

/// Pack balanced blocks into messages within Telegram's size cap.
fn pack(blocks: &[String], max: usize) -> Vec<String> {
    let mut msgs: Vec<String> = Vec::new();
    let mut cur = String::new();
    for b in blocks {
        if b.chars().count() > max {
            if !cur.is_empty() {
                msgs.push(std::mem::take(&mut cur));
            }
            msgs.extend(split_chunks(b, max));
            continue;
        }
        let sep = if cur.is_empty() { 0 } else { 2 };
        if cur.chars().count() + sep + b.chars().count() > max {
            msgs.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push_str("\n\n");
        }
        cur.push_str(b);
    }
    if !cur.is_empty() {
        msgs.push(cur);
    }
    msgs
}

/// Split on char boundaries so a long answer survives Telegram's 4096 cap.
fn split_chunks(s: &str, max: usize) -> Vec<String> {
    if s.chars().count() <= max {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars() {
        if cur.chars().count() == max {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

const HELP: &str = "🫏 Soy Don Rucio, el burro que se leyó el manual. \
Pregúntame lo que quieras sobre Rucio (instalación, configuración, eMule/Kad, \
descargas, compartir…) y te respondo con lo que pone en la documentación \
oficial. En un grupo, menciona al bot o usa «/rucio tu pregunta». Solo sé lo que \
dicen los docs; para lo demás, al repo: https://github.com/ogarcia/rucio";

fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let cfg = Config::from_env()?;

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(cfg.poll_timeout + 30)))
        .user_agent(UA)
        .build()
        .into();

    // `--ask "<question>"` answers once on the CLI (no Telegram needed) — handy
    // for testing the docs + model path.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--ask") {
        let question = args[1..].join(" ");
        if question.trim().is_empty() {
            return Err(anyhow!("usage: donrucio --ask \"<question>\""));
        }
        let docs = load_docs(&agent, &cfg)?;
        let sections = split_sections(&docs);
        let ctx = select_context(&sections, &question, cfg.context_chars);
        let messages = vec![
            json!({"role": "system", "content": system_prompt(&ctx)}),
            json!({"role": "user", "content": question}),
        ];
        println!("{}", ask_groq(&agent, &cfg, messages)?);
        return Ok(());
    }

    if cfg.tg_token.is_empty() {
        return Err(anyhow!(
            "TELEGRAM_BOT_TOKEN is required to run the bot (get one from @BotFather)"
        ));
    }

    let (bot_id, bot_username) = tg_get_me(&agent, &cfg).unwrap_or_else(|e| {
        eprintln!("warn: getMe failed ({e}); group @mentions won't be recognised");
        (0, String::new())
    });
    eprintln!("bot identity: @{bot_username} (id {bot_id})");
    tg_set_commands(&agent, &cfg);

    let mut sections = split_sections(&load_docs(&agent, &cfg)?);
    let mut last_refresh = Instant::now();
    eprintln!("donrucio ready (model {}). Polling…", cfg.groq_model);

    // chat_id → (last activity, recent messages)
    let mut history: HashMap<i64, (Instant, Vec<Value>)> = HashMap::new();
    let mut offset: i64 = 0;
    let mut poll_errors: u32 = 0;
    loop {
        // Periodic docs refresh so the bot tracks the repo without a restart.
        if let Some(every) = cfg.refresh {
            if last_refresh.elapsed() >= every {
                if let Ok(fresh) = load_docs(&agent, &cfg) {
                    sections = split_sections(&fresh);
                }
                last_refresh = Instant::now();
            }
        }

        let updates = match tg_get_updates(&agent, &cfg, offset) {
            Ok(u) => {
                poll_errors = 0;
                u
            }
            Err(e) => {
                // A lone getUpdates hiccup (a timeout or network blip) is normal
                // with long polling, so stay quiet; only warn once it is clearly
                // persistent, which points to a real outage.
                poll_errors += 1;
                if poll_errors == 3 {
                    eprintln!("warn: getUpdates failing repeatedly: {e}");
                }
                std::thread::sleep(Duration::from_secs(3));
                continue;
            }
        };

        // Drop chats idle beyond the reset window: they start fresh, and the
        // map does not grow with abandoned conversations. Triggered by incoming
        // traffic, so there is no background sweep.
        if let Some(reset) = cfg.idle_reset {
            if !updates.is_empty() {
                history.retain(|_, (seen, _)| seen.elapsed() < reset);
            }
        }

        for up in updates {
            if let Some(id) = up.get("update_id").and_then(Value::as_i64) {
                offset = offset.max(id + 1);
            }
            let msg = match up.get("message") {
                Some(m) => m,
                None => continue,
            };
            let chat_id = match msg.pointer("/chat/id").and_then(Value::as_i64) {
                Some(c) => c,
                None => continue,
            };
            let text_raw = msg.get("text").and_then(Value::as_str).unwrap_or("").trim();
            if text_raw.is_empty() {
                continue;
            }

            // In groups the bot (privacy mode on) only receives mentions, replies
            // to it and commands; answer only when actually addressed.
            let chat_type = msg
                .pointer("/chat/type")
                .and_then(Value::as_str)
                .unwrap_or("private");
            let is_group = matches!(chat_type, "group" | "supergroup");

            // Parse a leading command, tolerating the "@BotName" suffix.
            let first = text_raw.split_whitespace().next().unwrap_or("");
            let cmd = if first.starts_with('/') {
                first.split('@').next().unwrap_or("").to_string()
            } else {
                String::new()
            };

            if cmd == "/start" || cmd == "/help" {
                let _ = send_msg(&agent, &cfg, chat_id, HELP, false);
                continue;
            }

            // Work out the question. A command addressed to the bot (/rucio …)
            // reaches it even with privacy mode on, where plain @mentions don't;
            // a plain message is answered only when the bot is addressed.
            let question = if matches!(cmd.as_str(), "/rucio" | "/ask" | "/pregunta") {
                text_raw[first.len()..].trim().to_string()
            } else if !cmd.is_empty() {
                continue; // unknown command
            } else {
                if is_group {
                    let mentioned = !bot_username.is_empty()
                        && text_raw
                            .to_lowercase()
                            .contains(&format!("@{}", bot_username.to_lowercase()));
                    let reply_to_bot = msg
                        .pointer("/reply_to_message/from/id")
                        .and_then(Value::as_i64)
                        == Some(bot_id);
                    if !mentioned && !reply_to_bot {
                        continue;
                    }
                }
                // Strip the @mention token so the question reads cleanly.
                let mention = format!("@{bot_username}");
                text_raw
                    .split_whitespace()
                    .filter(|t| bot_username.is_empty() || !t.eq_ignore_ascii_case(&mention))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let question = question.trim().to_string();
            if question.is_empty() {
                let _ = send_msg(&agent, &cfg, chat_id, HELP, false);
                continue;
            }
            let text = question.as_str();

            tg_typing(&agent, &cfg, chat_id);
            let now = Instant::now();
            let hist = {
                let e = history.entry(chat_id).or_insert_with(|| (now, Vec::new()));
                e.0 = now; // mark activity
                &mut e.1
            };
            // Include the previous question in the retrieval query so follow-ups
            // ("¿y en Docker?") still fetch the right section.
            let prev_user = hist
                .iter()
                .rev()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                .and_then(|m| m.get("content").and_then(Value::as_str))
                .unwrap_or("")
                .to_string();
            let ctx = select_context(&sections, &format!("{prev_user} {text}"), cfg.context_chars);
            let mut messages = vec![json!({"role": "system", "content": system_prompt(&ctx)})];
            messages.extend(recent_history(hist, HISTORY_CHARS));
            messages.push(json!({"role": "user", "content": text}));

            match ask_groq(&agent, &cfg, messages) {
                Ok(answer) => {
                    tg_reply(&agent, &cfg, chat_id, &answer);
                    hist.push(json!({"role": "user", "content": text}));
                    hist.push(json!({"role": "assistant", "content": truncate(&answer, 800)}));
                    let keep = cfg.history_messages.max(2);
                    while hist.len() > keep {
                        hist.remove(0);
                    }
                }
                Err(e) => {
                    eprintln!("warn: groq: {e}");
                    let _ = send_msg(
                        &agent,
                        &cfg,
                        chat_id,
                        "Uy, ahora mismo no alcanzo al modelo. Prueba de nuevo en un momento.",
                        false,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_common_markdown() {
        let md = "Usa **`network.upload_limit_kbps`** para limitar.\n\n\
```sh\nrucio config set network.upload_limit_kbps 500\n```\n\n\
- Pon `0` para ilimitado\n- Reinicia el *daemon*\n\n\
Más info: [docs](https://x/y?a=1&b=2)";
        let joined = md_to_blocks(md).join("\n---\n");
        eprintln!("{joined}");
        assert!(joined.contains("<b><code>network.upload_limit_kbps</code></b>"));
        assert!(joined.contains("<pre><code class=\"language-sh\">"));
        assert!(joined.contains("• Pon <code>0</code>"));
        assert!(joined.contains("a=1&amp;b=2"));
        assert!(!joined.contains("**"));
        for tag in ["b", "i", "code", "pre", "a", "s"] {
            let open = joined.matches(&format!("<{tag}")).count();
            let close = joined.matches(&format!("</{tag}>")).count();
            assert_eq!(open, close, "unbalanced <{tag}>");
        }
    }
}
