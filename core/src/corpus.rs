use crate::ledger::{self, Segment, Source, Unit};
use crate::llm::{LlmClient, Message};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const MAX_CHUNKS: usize = 400;
const MAX_BLOCK_CHARS: usize = 1200;
const MAX_SITE_LINKS: usize = 40;
const DEFAULT_SITE_PAGES: usize = 40;

#[derive(Debug, Clone, Default)]
pub struct Walk {
    pub visited: BTreeSet<(String, usize)>,
    pub failed: Vec<(String, usize, String)>,
    pub notes: Vec<String>,
    pub dropped: Vec<(String, String)>,
}

pub trait Extract {
    fn extract(
        &self,
        question: &str,
        source_id: &str,
        unit: Unit,
        seg: &Segment,
    ) -> Result<Vec<String>>;
}

pub struct LlmExtract {
    client: LlmClient,
}

impl LlmExtract {
    pub fn from_config() -> Result<Self> {
        let paths = crate::config::Paths::resolve()?;
        crate::config::ensure_env_file(&paths.env_file)?;
        let layers = crate::config::Layers::load(&paths)?;
        if layers.llm_summary().provider == "нет" {
            bail!("LLM не настроена: задайте провайдера и его API-ключ (или LOCAL_BASE_URL)");
        }
        let settings = crate::config::Settings::load(&layers)?;
        Ok(Self {
            client: LlmClient::new(&settings.router_url, &settings.router_token),
        })
    }
}

impl Extract for LlmExtract {
    fn extract(
        &self,
        question: &str,
        source_id: &str,
        unit: Unit,
        seg: &Segment,
    ) -> Result<Vec<String>> {
        let citation = ledger::citation_for_segment(source_id, unit, seg);
        let system = format!(
            "Ты извлекаешь факты только из одного данного фрагмента. \
             Выпиши по-русски только факты, буквально присутствующие в нём и относящиеся к вопросу. \
             Каждый факт — отдельная строка, в конце строки поставь точную ссылку {citation}. \
             Не добавляй знаний извне, выводов и догадок. Если подходящих фактов нет, ответь ровно НЕТ."
        );
        let user = format!("ВОПРОС: {question}\nФРАГМЕНТ:\n{}", seg.text);
        let response = self.client.complete(
            &[Message::system(system), Message::user(user)],
            "corpus_extract",
            false,
            0.0,
        )?;
        let text = response.text.trim();
        if text == "НЕТ" {
            return Ok(Vec::new());
        }
        Ok(text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && *line != "НЕТ")
            .map(str::to_string)
            .collect())
    }
}

pub fn text_file(path: impl AsRef<Path>) -> Result<Source> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("не удалось прочитать {}", path.display()))?;
    let paragraphs = text_paragraphs(&text);
    let blocks = merge_blocks(paragraphs);
    let id = format!("file_{}", safe_id(&path.to_string_lossy()));
    Ok(block_source(
        id,
        path.to_string_lossy().into_owned(),
        blocks,
    ))
}

pub fn web_page(url: &str) -> Result<Source> {
    let html = fetch_html(url)?;
    let blocks = html_to_blocks(&html);
    let id = format!("web_{}", safe_id(url));
    Ok(block_source(id, url.to_string(), blocks))
}

pub fn html_to_blocks(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut cursor = 0;
    let mut current = String::new();
    let mut paragraphs = Vec::new();

    while let Some(open_offset) = html[cursor..].find('<') {
        let open = cursor + open_offset;
        current.push_str(&decode_entities(&html[cursor..open]));
        let Some(close_offset) = html[open..].find('>') else {
            current.push_str(&decode_entities(&html[open..]));
            cursor = html.len();
            break;
        };
        let close = open + close_offset;
        let tag = html[open + 1..close].trim();
        let normalized = tag.trim_start_matches('/').trim().to_ascii_lowercase();
        let name = normalized
            .split(|ch: char| ch.is_whitespace() || ch == '/' || ch == '>')
            .next()
            .unwrap_or("");
        let closing = tag.starts_with('/');

        if !closing && matches!(name, "script" | "style" | "noscript") {
            let end_tag = format!("</{name}");
            if let Some(end_offset) = lower[close + 1..].find(&end_tag) {
                let end = close + 1 + end_offset;
                cursor = lower[end..]
                    .find('>')
                    .map(|offset| end + offset + 1)
                    .unwrap_or(html.len());
            } else {
                cursor = html.len();
            }
            continue;
        }

        if is_block_tag(name) {
            flush_html_block(&mut current, &mut paragraphs);
        }
        cursor = close + 1;
    }
    if cursor < html.len() {
        current.push_str(&decode_entities(&html[cursor..]));
    }
    flush_html_block(&mut current, &mut paragraphs);
    limit_blocks(paragraphs)
}

fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div" | "li" | "br" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
    )
}

fn flush_html_block(current: &mut String, output: &mut Vec<String>) {
    let text = current.split_whitespace().collect::<Vec<_>>().join(" ");
    if !text.is_empty() {
        output.push(text);
    }
    current.clear();
}

fn text_paragraphs(text: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            flush_html_block(&mut current, &mut output);
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(line);
        }
    }
    flush_html_block(&mut current, &mut output);
    output
}

fn merge_blocks(paragraphs: Vec<String>) -> Vec<String> {
    let mut output = Vec::new();
    let mut current = String::new();
    for paragraph in paragraphs {
        let paragraph = paragraph.split_whitespace().collect::<Vec<_>>().join(" ");
        if paragraph.is_empty() {
            continue;
        }
        if paragraph.chars().count() > MAX_BLOCK_CHARS {
            if !current.is_empty() {
                output.push(std::mem::take(&mut current));
            }
            output.extend(split_chars(&paragraph, MAX_BLOCK_CHARS));
            continue;
        }
        let combined_len =
            current.chars().count() + usize::from(!current.is_empty()) + paragraph.chars().count();
        if combined_len > MAX_BLOCK_CHARS {
            output.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(&paragraph);
    }
    if !current.is_empty() {
        output.push(current);
    }
    output
}

fn limit_blocks(blocks: Vec<String>) -> Vec<String> {
    blocks
        .into_iter()
        .flat_map(|block| {
            let block = block.split_whitespace().collect::<Vec<_>>().join(" ");
            if block.chars().count() > MAX_BLOCK_CHARS {
                split_chars(&block, MAX_BLOCK_CHARS)
            } else if block.is_empty() {
                Vec::new()
            } else {
                vec![block]
            }
        })
        .collect()
}

fn split_chars(text: &str, limit: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(limit)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn block_source(id: String, url: String, blocks: Vec<String>) -> Source {
    let segments: Vec<Segment> = blocks
        .into_iter()
        .enumerate()
        .map(|(index, text)| Segment {
            start: index as u32,
            end: index as u32,
            text,
        })
        .collect();
    Source {
        id,
        url,
        unit: Unit::Block,
        duration: segments.len() as u32,
        segments,
    }
}

fn safe_id(value: &str) -> String {
    let mut result = String::new();
    let mut underscore = false;
    for ch in value.to_ascii_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            result.push(ch);
            underscore = false;
        } else if !underscore && !result.is_empty() {
            result.push('_');
            underscore = true;
        }
    }
    while result.ends_with('_') {
        result.pop();
    }
    if result.is_empty() {
        "source".to_string()
    } else {
        result
    }
}

fn fetch_html(url: &str) -> Result<String> {
    ureq::get(url)
        .call()
        .with_context(|| format!("не удалось загрузить {url}"))?
        .into_string()
        .with_context(|| format!("не удалось прочитать HTML {url}"))
}

pub fn same_host_links(html: &str, base: &str) -> Vec<String> {
    let Some(base_parts) = parse_url(base) else {
        return Vec::new();
    };
    let lower = html.to_ascii_lowercase();
    let mut cursor = 0;
    let mut links = Vec::new();
    let mut seen = HashSet::new();
    while let Some(offset) = lower[cursor..].find("<a") {
        let start = cursor + offset;
        let after = start + 2;
        if lower
            .as_bytes()
            .get(after)
            .is_some_and(|ch| !ch.is_ascii_whitespace() && *ch != b'>')
        {
            cursor = after;
            continue;
        }
        let Some(end_offset) = html[start..].find('>') else {
            break;
        };
        let end = start + end_offset;
        if let Some(href) = extract_href(&html[start..=end]) {
            if let Some(url) = resolve_link(&href, &base_parts) {
                if seen.insert(url.clone()) {
                    links.push(url);
                    if links.len() == MAX_SITE_LINKS {
                        break;
                    }
                }
            }
        }
        cursor = end + 1;
    }
    links
}

fn extract_href(tag: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let tail = &tag[2..];
    let lower_tail = &lower[2..];
    let mut cursor = 0;
    while let Some(offset) = lower_tail[cursor..].find("href") {
        let start = cursor + offset;
        let before_ok = start == 0 || lower_tail.as_bytes()[start - 1].is_ascii_whitespace();
        let mut index = start + 4;
        let after_ok = lower_tail
            .as_bytes()
            .get(index)
            .is_some_and(|ch| ch.is_ascii_whitespace() || *ch == b'=');
        if !before_ok || !after_ok {
            cursor = index;
            continue;
        }
        while lower_tail
            .as_bytes()
            .get(index)
            .is_some_and(|ch| ch.is_ascii_whitespace())
        {
            index += 1;
        }
        if lower_tail.as_bytes().get(index) != Some(&b'=') {
            cursor = index;
            continue;
        }
        index += 1;
        while lower_tail
            .as_bytes()
            .get(index)
            .is_some_and(|ch| ch.is_ascii_whitespace())
        {
            index += 1;
        }
        let value = if matches!(lower_tail.as_bytes().get(index), Some(b'"' | b'\'')) {
            let quote = lower_tail.as_bytes()[index] as char;
            let value_start = index + 1;
            let end = lower_tail[value_start..].find(quote)?;
            &tail[value_start..value_start + end]
        } else {
            let value_start = index;
            let end = lower_tail[value_start..]
                .find(|ch: char| ch.is_ascii_whitespace() || ch == '>')
                .unwrap_or(lower_tail.len() - value_start);
            &tail[value_start..value_start + end]
        };
        return Some(decode_entities(value).trim().to_string());
    }
    None
}

#[derive(Debug)]
struct UrlParts {
    scheme: String,
    authority: String,
    host: String,
    path_query: String,
}

fn parse_url(url: &str) -> Option<UrlParts> {
    let clean = url.split('#').next()?.trim();
    let (scheme, rest) = clean.split_once("://")?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return None;
    }
    let boundary = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..boundary];
    if authority.is_empty() {
        return None;
    }
    let host = authority
        .rsplit('@')
        .next()?
        .split(':')
        .next()?
        .to_ascii_lowercase();
    let path_query = if boundary == rest.len() {
        "/".to_string()
    } else if rest[boundary..].starts_with('?') {
        format!("/{}", &rest[boundary..])
    } else {
        rest[boundary..].to_string()
    };
    Some(UrlParts {
        scheme: scheme.to_ascii_lowercase(),
        authority: authority.to_string(),
        host,
        path_query,
    })
}

fn resolve_link(href: &str, base: &UrlParts) -> Option<String> {
    let href = href.trim();
    if href.is_empty()
        || href.starts_with('#')
        || ["mailto:", "tel:", "javascript:", "data:"]
            .iter()
            .any(|scheme| href.to_ascii_lowercase().starts_with(scheme))
    {
        return None;
    }
    let origin = format!("{}://{}", base.scheme, base.authority);
    let absolute = if href.starts_with("//") {
        format!("{}:{href}", base.scheme)
    } else if href.contains("://") {
        href.to_string()
    } else if href.starts_with('/') {
        format!("{origin}{href}")
    } else {
        let base_path = base.path_query.split('?').next().unwrap_or("/");
        if href.starts_with('?') {
            format!("{origin}{base_path}{href}")
        } else {
            let directory = base_path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
            format!("{origin}{directory}/{href}")
        }
    };
    let no_fragment = absolute.split('#').next()?;
    let parts = parse_url(no_fragment)?;
    if parts.host != base.host {
        return None;
    }
    Some(normalize_url(no_fragment, &parts))
}

fn normalize_url(url: &str, parts: &UrlParts) -> String {
    let url = url.split('#').next().unwrap_or(url);
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or("");
    let path_query = &rest[parts.authority.len()..];
    let (path, query) = path_query
        .split_once('?')
        .map(|(path, query)| (path, Some(query)))
        .unwrap_or((path_query, None));
    let mut stack = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            value => stack.push(value),
        }
    }
    let normalized_path = format!("/{}", stack.join("/"));
    format!(
        "{}://{}{}{}",
        parts.scheme,
        parts.authority,
        normalized_path,
        query.map(|query| format!("?{query}")).unwrap_or_default()
    )
}

pub fn site(root_url: &str, max_pages: usize) -> Result<Vec<Source>> {
    let Some(root) = parse_url(root_url) else {
        bail!("нужен абсолютный HTTP(S) URL сайта");
    };
    let root_url = normalize_url(root_url, &root);
    let limit = max_pages.max(1);
    let mut queue = VecDeque::from([root_url.clone()]);
    let mut seen = HashSet::new();
    let mut sources = Vec::new();
    while let Some(url) = queue.pop_front() {
        if sources.len() >= limit || !seen.insert(url.clone()) {
            continue;
        }
        let html = fetch_html(&url)?;
        let blocks = html_to_blocks(&html);
        sources.push(block_source(
            format!("page_{}", sources.len() + 1),
            url.clone(),
            blocks,
        ));
        if sources.len() < limit {
            for link in same_host_links(&html, &url) {
                if !seen.contains(&link) {
                    queue.push_back(link);
                }
            }
        }
    }
    Ok(sources)
}

pub fn collector(command: &str, arg: &str) -> Result<Vec<Source>> {
    run_collector(command, &[arg.to_string()])
}

pub fn run_collector(command: &str, args: &[String]) -> Result<Vec<Source>> {
    let mut child = Command::new(command)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("не удалось запустить сборщик {command}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("не удалось прочитать stdout сборщика"))?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let started = Instant::now();
    let timeout = Duration::from_secs(15 * 60);
    let status = loop {
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            bail!("сборщик не завершился за 15 минут");
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err(anyhow!("ошибка ожидания сборщика: {error}"));
            }
        }
    };
    let output = reader
        .join()
        .map_err(|_| anyhow!("ошибка чтения stdout сборщика"))??;
    if !status.success() {
        bail!("сборщик завершился с кодом {status}");
    }
    serde_json::from_slice(&output).context("сборщик вернул некорректный JSON [Source]")
}

pub fn load_sources(specs: &[String], collector_override: Option<&str>) -> Result<Vec<Source>> {
    let mut sources = Vec::new();
    for spec in specs {
        let (kind, arg) = spec
            .split_once(':')
            .ok_or_else(|| anyhow!("источник должен иметь вид type:ARG: {spec}"))?;
        match kind {
            "file" => sources.push(text_file(arg)?),
            "web" => sources.push(web_page(arg)?),
            "site" => sources.extend(site(arg, DEFAULT_SITE_PAGES)?),
            "collector" => {
                let command = collector_override
                    .map(str::to_string)
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        anyhow!("сборщик видео не настроен: задайте PCAGENT_COLLECTOR")
                    })?;
                sources.extend(collector(&command, arg)?);
            }
            _ => bail!("неизвестный тип источника {kind}"),
        }
    }
    let mut ids = HashSet::new();
    for source in &sources {
        if !ids.insert(source.id.as_str()) {
            bail!("повторяется id источника {}", source.id);
        }
        if source.id.is_empty()
            || !source
                .id
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        {
            bail!("недопустимый id источника {}", source.id);
        }
    }
    Ok(sources)
}

pub fn walk(question: &str, sources: &[Source], extractor: &dyn Extract) -> Result<Walk> {
    let count: usize = sources.iter().map(|source| source.segments.len()).sum();
    if count > MAX_CHUNKS {
        bail!("корпус больше лимита: {count} кусков");
    }
    let mut ids = HashSet::new();
    for source in sources {
        if !ids.insert(source.id.as_str()) {
            bail!("повторяется id источника {}", source.id);
        }
    }

    let mut output = Walk::default();
    for source in sources {
        for segment in &source.segments {
            let position = segment.start as usize;
            let result = extractor
                .extract(question, &source.id, source.unit, segment)
                .or_else(|first_error| {
                    extractor
                        .extract(question, &source.id, source.unit, segment)
                        .map_err(|retry_error| {
                            anyhow!("{first_error}; повторная попытка: {retry_error}")
                        })
                });
            let notes = match result {
                Ok(notes) => notes,
                Err(error) => {
                    output
                        .failed
                        .push((source.id.clone(), position, format!("{error:#}")));
                    continue;
                }
            };
            output.visited.insert((source.id.clone(), position));
            let chunk = Source {
                segments: vec![segment.clone()],
                ..source.clone()
            };
            for note in notes {
                let note = note.trim().to_string();
                if note.is_empty() || note == "НЕТ" {
                    continue;
                }
                match ledger::verify_statement(&note, &[chunk.clone()]) {
                    Ok(_) => output.notes.push(note),
                    Err(reason) => output.dropped.push((note, reason.to_string())),
                }
            }
        }
    }
    Ok(output)
}

pub fn back_gate(
    sources: &[Source],
    walk: &Walk,
    report: &str,
) -> std::result::Result<String, serde_json::Value> {
    let incomplete: Vec<serde_json::Value> = sources
        .iter()
        .filter_map(|source| {
            ledger::complete(source)
                .err()
                .map(|reason| serde_json::json!({"id":source.id,"why":reason}))
        })
        .collect();
    let mut unvisited = Vec::new();
    for source in sources {
        for segment in &source.segments {
            let position = segment.start as usize;
            if !walk.visited.contains(&(source.id.clone(), position)) {
                unvisited.push(format!(
                    "{}@{}",
                    source.id,
                    ledger::locator_for(source.unit, segment)
                ));
            }
        }
    }
    let failed: Vec<serde_json::Value> = walk
        .failed
        .iter()
        .map(|(id, position, why)| {
            let locator = sources
                .iter()
                .find(|source| source.id == *id)
                .and_then(|source| {
                    source
                        .segments
                        .iter()
                        .find(|segment| segment.start as usize == *position)
                        .map(|segment| ledger::locator_for(source.unit, segment))
                })
                .unwrap_or_else(|| position.to_string());
            serde_json::json!({"chunk":format!("{id}@{locator}"),"why":why})
        })
        .collect();
    let check = ledger::check_report(report, sources);
    let rejected: Vec<serde_json::Value> = check
        .rejected
        .iter()
        .map(|(statement, why)| serde_json::json!({"statement":statement,"why":why}))
        .collect();
    let mut reasons = serde_json::Map::new();
    if !incomplete.is_empty() {
        reasons.insert("incomplete".into(), serde_json::Value::Array(incomplete));
    }
    if !unvisited.is_empty() {
        reasons.insert(
            "unvisited".into(),
            serde_json::Value::Array(unvisited.into_iter().map(Into::into).collect()),
        );
    }
    if !failed.is_empty() {
        reasons.insert("failed".into(), serde_json::Value::Array(failed));
    }
    if !rejected.is_empty() {
        reasons.insert("rejected".into(), serde_json::Value::Array(rejected));
    }
    if check.verified.is_empty() {
        reasons.insert(
            "reason".into(),
            "отчёт не содержит проверенных утверждений".into(),
        );
    }
    if reasons.is_empty() {
        Ok(check.verified_text())
    } else {
        reasons.insert("status".into(), "open".into());
        reasons.insert("verified_report".into(), check.verified_text().into());
        Err(serde_json::Value::Object(reasons))
    }
}

fn decode_entities(text: &str) -> String {
    let mut output = String::new();
    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find('&') {
        let start = cursor + offset;
        output.push_str(&text[cursor..start]);
        let Some(end_offset) = text[start..].find(';') else {
            output.push_str(&text[start..]);
            return output;
        };
        let end = start + end_offset;
        let entity = &text[start + 1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => {
                entity[1..].parse::<u32>().ok().and_then(char::from_u32)
            }
            _ => None,
        };
        if let Some(ch) = decoded {
            output.push(ch);
        } else {
            output.push_str(&text[start..=end]);
        }
        cursor = end + 1;
    }
    output.push_str(&text[cursor..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeExtract;

    impl Extract for FakeExtract {
        fn extract(
            &self,
            _question: &str,
            source_id: &str,
            unit: Unit,
            seg: &Segment,
        ) -> Result<Vec<String>> {
            let citation = ledger::citation_for_segment(source_id, unit, seg);
            let fact = seg.text.trim().trim_end_matches('.');
            Ok(vec![
                format!("{fact} {citation}."),
                format!("{fact} 999 {citation}."),
            ])
        }
    }

    struct OneErrorExtract(AtomicUsize);

    impl Extract for OneErrorExtract {
        fn extract(
            &self,
            question: &str,
            source_id: &str,
            unit: Unit,
            seg: &Segment,
        ) -> Result<Vec<String>> {
            if seg.start == 1 {
                self.0.fetch_add(1, Ordering::SeqCst);
                bail!("тестовая ошибка извлечения")
            }
            FakeExtract.extract(question, source_id, unit, seg)
        }
    }

    struct CountingExtract(AtomicUsize);

    impl Extract for CountingExtract {
        fn extract(
            &self,
            _question: &str,
            _source_id: &str,
            _unit: Unit,
            _seg: &Segment,
        ) -> Result<Vec<String>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    fn block_source(id: &str, texts: &[&str]) -> Source {
        let segments: Vec<Segment> = texts
            .iter()
            .enumerate()
            .map(|(index, text)| Segment {
                start: index as u32,
                end: index as u32,
                text: (*text).to_string(),
            })
            .collect();
        Source {
            id: id.to_string(),
            url: format!("file:{id}"),
            unit: Unit::Block,
            duration: segments.len() as u32,
            segments,
        }
    }

    #[test]
    fn walk_visits_every_chunk_and_keeps_only_verified_notes() {
        let sources: Vec<Source> = (0..30)
            .map(|index| {
                let final_text = if index == 28 {
                    "Ключевой вывод найден в конце двадцать восьмого материала."
                } else {
                    "Материал содержит подробное описание важного процесса."
                };
                block_source(
                    &format!("unit_{index}"),
                    &["В начале материала описана общая тема.", final_text],
                )
            })
            .collect();

        let walked = walk("Какой главный вывод?", &sources, &FakeExtract).unwrap();
        assert_eq!(walked.visited.len(), 60);
        assert!(walked.visited.contains(&("unit_28".to_string(), 1)));
        assert!(walked.visited.contains(&("unit_29".to_string(), 1)));
        assert!(walked
            .notes
            .iter()
            .any(|note| note.contains("Ключевой вывод найден")));
        assert_eq!(walked.dropped.len(), 60);
        assert!(walked
            .dropped
            .iter()
            .all(|(_, why)| why.contains("число 999")));
    }

    #[test]
    fn failed_chunk_is_retried_and_back_gate_lists_it() {
        let source = block_source(
            "transcript",
            &[
                "Надёжный вывод подтверждён текстом источника.",
                "Второй фрагмент также содержит подробный текст.",
            ],
        );
        let extractor = OneErrorExtract(AtomicUsize::new(0));
        let walked = walk("Что подтверждено?", &[source.clone()], &extractor).unwrap();
        assert_eq!(extractor.0.load(Ordering::SeqCst), 2);
        assert!(walked.visited.contains(&("transcript".to_string(), 0)));
        assert!(!walked.visited.contains(&("transcript".to_string(), 1)));
        assert_eq!(walked.failed.len(), 1);

        let report = "Надёжный вывод подтверждён текстом источника [transcript@b0].";
        let reasons = back_gate(&[source], &walked, report).unwrap_err();
        assert!(reasons["unvisited"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "transcript@b1"));
    }

    #[test]
    fn back_gate_rejects_fabricated_report_claims() {
        let source = block_source(
            "notes",
            &["Документ подтверждает важный вывод для команды."],
        );
        let walked = walk("Что сообщает документ?", &[source.clone()], &FakeExtract).unwrap();
        let reasons = back_gate(
            &[source],
            &walked,
            "Документ подтверждает важный вывод 999 для команды [notes@b0].",
        )
        .unwrap_err();
        assert!(!reasons["rejected"].as_array().unwrap().is_empty());
    }

    #[test]
    fn back_gate_allows_sources_not_needed_by_the_report() {
        let sources = vec![
            block_source("first", &["Первый источник содержит ключевой вывод."]),
            block_source(
                "second",
                &["Второй источник содержит дополнительный контекст."],
            ),
        ];
        let walked = walk("Что сообщает корпус?", &sources, &FakeExtract).unwrap();
        let report = "Первый источник содержит ключевой вывод [first@b0].";
        assert!(back_gate(&sources, &walked, report).is_ok());
    }

    #[test]
    fn back_gate_refuses_incomplete_sources_and_reports_without_verified_claims() {
        let mut incomplete = block_source(
            "partial",
            &["Документ содержит важное проверяемое утверждение."],
        );
        incomplete.duration = 2;
        let walked = walk(
            "Что содержит документ?",
            &[incomplete.clone()],
            &FakeExtract,
        )
        .unwrap();
        let report = "Документ содержит важное проверяемое утверждение [partial@b0].";
        let reasons = back_gate(&[incomplete], &walked, report).unwrap_err();
        assert!(!reasons["incomplete"].as_array().unwrap().is_empty());

        let complete = block_source(
            "complete",
            &["Документ содержит важное проверяемое утверждение."],
        );
        let walked = walk("Что содержит документ?", &[complete.clone()], &FakeExtract).unwrap();
        let reasons = back_gate(&[complete], &walked, "Заголовок").unwrap_err();
        assert_eq!(
            reasons["reason"],
            "отчёт не содержит проверенных утверждений"
        );
    }

    #[test]
    fn html_adapter_drops_script_and_style_content() {
        let blocks = html_to_blocks(
            "<h1>Заголовок &amp; вывод</h1><script>секретный текст</script><p>Нужный факт.</p><style>невидимый стиль</style><noscript>нет скриптов</noscript>",
        );
        assert!(blocks
            .iter()
            .any(|block| block.contains("Заголовок & вывод")));
        assert!(blocks.iter().any(|block| block.contains("Нужный факт")));
        assert_eq!(blocks.len(), 2);
        let text = blocks.join(" ");
        assert!(!text.contains("секретный"));
        assert!(!text.contains("невидимый"));
        assert!(!text.contains("нет скриптов"));
    }

    #[test]
    fn html_blocks_are_limited_to_twelve_hundred_characters() {
        let html = format!("<p>{}</p>", "а".repeat(MAX_BLOCK_CHARS + 1));
        let blocks = html_to_blocks(&html);
        assert_eq!(blocks.len(), 2);
        assert!(blocks
            .iter()
            .all(|block| block.chars().count() <= MAX_BLOCK_CHARS));
    }

    #[test]
    fn same_host_links_deduplicate_and_filter_fragments_and_other_hosts() {
        let links = same_host_links(
            r##"<a href="/guide#top">A</a><a href="https://example.test/guide">B</a><a href="https://other.test/x">X</a><a href="#local">L</a><a href="mailto:hi@example.test">M</a><a href="tel:+100">T</a>"##,
            "https://example.test/start",
        );
        assert_eq!(links, vec!["https://example.test/guide"]);
    }

    #[test]
    fn oversized_corpus_is_rejected_before_extraction() {
        let texts = vec![
            "Фрагмент содержит проверяемый текст.";
            MAX_CHUNKS + 1
        ];
        let source = block_source("large", &texts);
        let extractor = CountingExtract(AtomicUsize::new(0));
        let error = walk("проверить весь корпус", &[source], &extractor)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            format!("корпус больше лимита: {} кусков", MAX_CHUNKS + 1)
        );
        assert_eq!(extractor.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn text_file_splits_paragraphs_into_bounded_blocks() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let path = std::env::temp_dir().join(format!(
            "pc-agent-corpus-{}-{}.txt",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let content = format!("Первый абзац текста.\n\n{}", "а".repeat(1201));
        std::fs::write(&path, content).unwrap();
        let source = text_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(source.unit, Unit::Block);
        assert_eq!(source.duration as usize, source.segments.len());
        assert!(source.segments.len() >= 3);
        assert!(source
            .segments
            .iter()
            .all(|segment| segment.text.chars().count() <= MAX_BLOCK_CHARS));
        assert!(ledger::complete(&source).is_ok());
    }
}

