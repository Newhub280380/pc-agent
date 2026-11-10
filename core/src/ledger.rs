use crate::minimal::{Lit, MinimalSet};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Default, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    #[default]
    Sec,
    Block,
    Page,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct Source {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub unit: Unit,
    pub duration: u32,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct Segment {
    pub start: u32,
    pub end: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Refusal {
    pub reason: String,
    pub minimal: MinimalSet,
}

impl Refusal {
    pub fn from_reason(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            minimal: MinimalSet::new(),
        }
    }

    pub fn with_cited(mut self, citations: &[Citation]) -> Self {
        for c in citations {
            self.minimal.push_lit(Lit::Ident(c.source_id.clone()));
        }
        self
    }
}

impl std::ops::Deref for Refusal {
    type Target = str;
    fn deref(&self) -> &str {
        &self.reason
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportCheck {
    pub verified: Vec<String>,
    pub rejected: Vec<(String, Refusal)>,
    pub uncited_sources: Vec<String>,
}

impl ReportCheck {
    pub fn verified_text(&self) -> String {
        self.verified.join("\n")
    }
}

pub fn complete(source: &Source) -> Result<(), String> {
    if source.segments.is_empty() {
        return Err(format!("{}: нет сегментов транскрипта", source.id));
    }

    if source.unit != Unit::Sec {
        let first = if source.unit == Unit::Page { 1 } else { 0 };
        let expected_last = if source.unit == Unit::Page {
            source.duration
        } else {
            source
                .duration
                .checked_sub(1)
                .ok_or_else(|| format!("{}: число блоков должно быть больше нуля", source.id))?
        };
        let mut expected_start = first;
        for segment in &source.segments {
            if segment.start > segment.end {
                return Err(format!("{}: начало сегмента позже его конца", source.id));
            }
            if segment.start != expected_start {
                return Err(format!(
                    "{}: пропуск в {:?}-единицах — ожидалось {}, получено {}",
                    source.id, source.unit, expected_start, segment.start
                ));
            }
            expected_start = segment
                .end
                .checked_add(1)
                .ok_or_else(|| format!("{}: индекс сегмента слишком велик", source.id))?;
        }
        let actual_last = source
            .segments
            .last()
            .map(|segment| segment.end)
            .unwrap_or(first);
        if actual_last != expected_last {
            return Err(format!(
                "{}: неполное покрытие {:?}-единиц — последний индекс {}, ожидался {}",
                source.id, source.unit, actual_last, expected_last
            ));
        }
        return Ok(());
    }

    let mut previous_start = None;
    let mut previous_end = None;
    for segment in &source.segments {
        if segment.start > segment.end {
            return Err(format!("{}: начало сегмента позже его конца", source.id));
        }
        if previous_start.is_some_and(|start| segment.start < start) {
            return Err(format!(
                "{}: сегменты транскрипта не отсортированы",
                source.id
            ));
        }
        if previous_end.is_some_and(|end| segment.start < end) {
            return Err(format!("{}: сегменты транскрипта пересекаются", source.id));
        }
        previous_start = Some(segment.start);
        previous_end = Some(segment.end);
    }

    let end = source
        .segments
        .iter()
        .map(|segment| segment.end)
        .max()
        .unwrap_or(0);
    let required_end = source.duration.saturating_sub(2);
    if end < required_end {
        return Err(format!(
            "{}: транскрипт обрывается на {} из {}",
            source.id,
            timecode(end),
            timecode(source.duration)
        ));
    }
    Ok(())
}

pub fn check_report(report: &str, sources: &[Source]) -> ReportCheck {
    let mut verified = Vec::new();
    let mut rejected = Vec::new();
    let mut cited_by_verified = HashSet::new();

    for statement in statements(report) {
        let (_, text, _) = extract_citations(&statement);
        if is_heading(&text) {
            continue;
        }
        let result = verify_statement(&statement, sources);
        match result {
            Ok(source_ids) => {
                cited_by_verified.extend(source_ids);
                verified.push(statement);
            }
            Err(reason) => rejected.push((statement, reason)),
        }
    }

    let uncited_sources = sources
        .iter()
        .filter(|source| !cited_by_verified.contains(source.id.as_str()))
        .map(|source| source.id.clone())
        .collect();

    ReportCheck {
        verified,
        rejected,
        uncited_sources,
    }
}

#[derive(Debug)]
struct Citation {
    source_id: String,
    locator: Locator,
}

#[derive(Debug)]
enum Locator {
    Sec(u32),
    Block(u32),
    Page(u32),
}

fn statements(report: &str) -> Vec<String> {
    let mut output = Vec::new();
    for line in report
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        let mut start = 0;
        for (index, (byte_index, ch)) in chars.iter().copied().enumerate() {
            let followed_by_space = chars
                .get(index + 1)
                .is_some_and(|(_, next)| next.is_whitespace());
            if matches!(ch, '.' | '!' | '?') && followed_by_space {
                let end = byte_index + ch.len_utf8();
                let statement = line[start..end].trim();
                if !statement.is_empty() {
                    output.push(statement.to_string());
                }
                start = end;
            }
        }
        let statement = line[start..].trim();
        if !statement.is_empty() {
            output.push(statement.to_string());
        }
    }
    output
}

fn extract_citations(statement: &str) -> (Vec<Citation>, String, Option<String>) {
    let mut citations = Vec::new();
    let mut text = String::new();
    let mut malformed = None;
    let mut cursor = 0;

    while let Some(open_offset) = statement[cursor..].find('[') {
        let open = cursor + open_offset;
        text.push_str(&statement[cursor..open]);
        let Some(close_offset) = statement[open + 1..].find(']') else {
            text.push_str(&statement[open..]);
            cursor = statement.len();
            break;
        };
        let close = open + 1 + close_offset;
        let bracket = &statement[open..=close];
        let content = &statement[open + 1..close];
        match parse_citation(content) {
            Some(citation) => citations.push(citation),
            None => {
                if content.starts_with("reel_") || content.contains('@') {
                    malformed.get_or_insert_with(|| bracket.to_string());
                }
                text.push_str(bracket);
            }
        }
        cursor = close + 1;
    }
    if cursor < statement.len() {
        text.push_str(&statement[cursor..]);
    }
    (citations, text, malformed)
}

fn parse_citation(value: &str) -> Option<Citation> {
    let (source_id, locator) = value.split_once('@')?;
    if source_id.is_empty()
        || !source_id
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    {
        return None;
    }
    let locator = if let Some(block) = locator.strip_prefix('b') {
        Locator::Block(parse_index(block)?)
    } else if let Some(page) = locator.strip_prefix('p') {
        Locator::Page(parse_index(page)?)
    } else {
        Locator::Sec(parse_timecode(locator)?)
    };
    Some(Citation {
        source_id: source_id.to_string(),
        locator,
    })
}

fn parse_index(value: &str) -> Option<u32> {
    if value.is_empty() || !value.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn parse_timecode(value: &str) -> Option<u32> {
    let parts: Vec<&str> = value.split(':').collect();
    match parts.as_slice() {
        [minutes, seconds] => {
            let minutes = parse_index(minutes)?;
            let seconds = parse_index(seconds)?;
            if seconds >= 60 {
                return None;
            }
            minutes.checked_mul(60)?.checked_add(seconds)
        }
        [hours, minutes, seconds] => {
            let hours = parse_index(hours)?;
            let minutes = parse_index(minutes)?;
            let seconds = parse_index(seconds)?;
            if minutes >= 60 || seconds >= 60 {
                return None;
            }
            hours
                .checked_mul(3600)?
                .checked_add(minutes.checked_mul(60)?)?
                .checked_add(seconds)
        }
        _ => None,
    }
}

fn is_heading(text: &str) -> bool {
    if !text.chars().any(char::is_alphabetic) {
        return true;
    }
    text.split_whitespace().count() <= 3 && !text.chars().any(|ch| ch.is_ascii_digit())
}

pub fn verify_statement(statement: &str, sources: &[Source]) -> Result<Vec<String>, Refusal> {
    let (citations, text, malformed) = extract_citations(statement);
    verify_statement_parts(&citations, &text, malformed.as_deref(), sources)
        .map_err(|reason| Refusal::from_reason(reason).with_cited(&citations))
}

fn verify_statement_parts(
    citations: &[Citation],
    text: &str,
    malformed: Option<&str>,
    sources: &[Source],
) -> Result<Vec<String>, String> {
    if let Some(citation) = malformed {
        return Err(format!("некорректная ссылка {citation}"));
    }
    if citations.is_empty() {
        return Err("нет ссылки вида [id@позиция]".to_string());
    }

    let mut window = Vec::new();
    let mut source_ids = Vec::new();
    for citation in citations {
        let source = sources
            .iter()
            .find(|source| source.id == citation.source_id)
            .ok_or_else(|| format!("источник {} отсутствует", citation.source_id))?;
        let position = match (source.unit, &citation.locator) {
            (Unit::Sec, Locator::Sec(time)) => *time,
            (Unit::Block, Locator::Block(block)) => *block,
            (Unit::Page, Locator::Page(page)) => *page,
            (unit, _) => {
                return Err(format!(
                    "формат ссылки не соответствует {:?}-источнику {}",
                    unit, citation.source_id
                ));
            }
        };
        let segment_index = source
            .segments
            .iter()
            .position(|segment| segment.start <= position && position <= segment.end)
            .ok_or_else(|| match source.unit {
                Unit::Sec => format!(
                    "таймкод {} не попадает в сегмент источника {}",
                    timecode(position),
                    citation.source_id
                ),
                unit => format!(
                    "позиция {} не попадает в сегмент источника {}",
                    format_locator(unit, position),
                    citation.source_id
                ),
            })?;
        let first = segment_index.saturating_sub(1);
        let last = (segment_index + 1).min(source.segments.len() - 1);
        window.extend(
            source.segments[first..=last]
                .iter()
                .map(|segment| segment.text.as_str()),
        );
        source_ids.push(citation.source_id.clone());
    }

    let window_text = window.join(" ");
    let window_numbers: HashSet<String> = digit_runs(&window_text).into_iter().collect();
    for number in digit_runs(text) {
        if !window_numbers.contains(&number) {
            return Err(format!("число {number} отсутствует в окне цитаты"));
        }
    }

    let words = content_words(text);
    if words.is_empty() {
        return Err("в утверждении нет проверяемых слов".to_string());
    }
    let window_prefixes: HashSet<String> = content_words(&window_text)
        .iter()
        .map(|word| prefix(word))
        .collect();
    let matched = words
        .iter()
        .filter(|word| window_prefixes.contains(&prefix(word)))
        .count();
    if matched * 2 < words.len() {
        return Err(format!(
            "недостаточно совпадающих слов: {matched} из {}",
            words.len()
        ));
    }
    Ok(source_ids)
}

fn content_words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|ch: char| !ch.is_alphabetic())
        .filter(|word| word.chars().count() >= 4)
        .map(str::to_string)
        .collect()
}

fn prefix(word: &str) -> String {
    word.chars().take(5).collect()
}

fn digit_runs(text: &str) -> Vec<String> {
    let mut output = Vec::new();
    let mut run = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            run.push(ch);
        } else if !run.is_empty() {
            output.push(std::mem::take(&mut run));
        }
    }
    if !run.is_empty() {
        output.push(run);
    }
    output
}

fn timecode(seconds: u32) -> String {
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            (seconds % 3600) / 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

fn format_locator(unit: Unit, position: u32) -> String {
    match unit {
        Unit::Sec => timecode(position),
        Unit::Block => format!("b{position}"),
        Unit::Page => format!("p{position}"),
    }
}

pub fn locator_for(unit: Unit, segment: &Segment) -> String {
    format_locator(unit, segment.start)
}

pub fn citation_for_segment(source_id: &str, unit: Unit, segment: &Segment) -> String {
    format!("[{}@{}]", source_id, locator_for(unit, segment))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str) -> Source {
        Source {
            id: id.to_string(),
            url: format!("https://example.test/{id}"),
            unit: Unit::Sec,
            duration: 100,
            segments: vec![
                Segment {
                    start: 0,
                    end: 30,
                    text: "В начале автомобиль едет по широкой городской улице.".to_string(),
                },
                Segment {
                    start: 30,
                    end: 60,
                    text: "Затем водитель поворачивает направо возле большого перекрёстка."
                        .to_string(),
                },
                Segment {
                    start: 60,
                    end: 100,
                    text: "В конце автомобиль останавливается рядом с жилым домом.".to_string(),
                },
            ],
        }
    }

    #[test]
    fn complete_accepts_full_transcript() {
        assert!(complete(&source("reel_1")).is_ok());
    }

    #[test]
    fn complete_rejects_missing_segments() {
        let mut source = source("reel_1");
        source.segments.clear();
        assert!(complete(&source).unwrap_err().contains("нет сегментов"));
    }

    #[test]
    fn complete_rejects_truncated_transcript() {
        let mut source = source("reel_3");
        source.segments[2].end = 72;
        let error = complete(&source).unwrap_err();
        assert!(error.contains("reel_3"));
        assert!(error.contains("01:12"));
        assert!(error.contains("01:40"));
    }

    #[test]
    fn complete_rejects_unsorted_or_overlapping_segments() {
        let mut unsorted = source("reel_1");
        unsorted.segments.swap(0, 1);
        assert!(complete(&unsorted)
            .unwrap_err()
            .contains("не отсортированы"));

        let mut overlapping = source("reel_1");
        overlapping.segments[1].start = 20;
        assert!(complete(&overlapping).unwrap_err().contains("пересекаются"));
    }

    #[test]
    fn old_source_json_defaults_to_seconds() {
        let source: Source = serde_json::from_str(
            r#"{"id":"reel_3","url":"https://example.test/3","duration":10,"segments":[{"start":0,"end":10,"text":"текст"}]}"#,
        )
        .unwrap();
        assert_eq!(source.unit, Unit::Sec);
    }

    #[test]
    fn block_and_page_sources_require_contiguous_full_coverage() {
        let block = Source {
            id: "text_file".to_string(),
            url: "file:test.txt".to_string(),
            unit: Unit::Block,
            duration: 2,
            segments: vec![
                Segment {
                    start: 0,
                    end: 0,
                    text: "Первый блок текста.".to_string(),
                },
                Segment {
                    start: 1,
                    end: 1,
                    text: "Второй блок текста.".to_string(),
                },
            ],
        };
        assert!(complete(&block).is_ok());
        let mut missing_block = block.clone();
        missing_block.segments[1].start = 2;
        missing_block.segments[1].end = 2;
        assert!(complete(&missing_block).unwrap_err().contains("пропуск"));

        let page = Source {
            id: "book_2024".to_string(),
            url: "file:book.pdf".to_string(),
            unit: Unit::Page,
            duration: 2,
            segments: vec![
                Segment {
                    start: 1,
                    end: 1,
                    text: "Текст первой страницы.".to_string(),
                },
                Segment {
                    start: 2,
                    end: 2,
                    text: "Текст второй страницы.".to_string(),
                },
            ],
        };
        assert!(complete(&page).is_ok());
    }

    #[test]
    fn report_verifies_a_cited_claim() {
        let check = check_report(
            "Водитель поворачивает направо возле перекрёстка [reel_1@00:40].",
            &[source("reel_1")],
        );
        assert_eq!(check.verified.len(), 1);
        assert!(check.rejected.is_empty());
        assert!(check.uncited_sources.is_empty());
        assert!(check.verified_text().contains("[reel_1@00:40]"));
    }

    #[test]
    fn report_rejects_a_number_not_in_the_citation_window() {
        let check = check_report(
            "Водитель поворачивает направо возле перекрёстка на 7-й улице [reel_1@00:40].",
            &[source("reel_1")],
        );
        assert!(check.verified.is_empty());
        assert!(check.rejected[0].1.contains("число 7"));
    }

    #[test]
    fn report_rejects_a_wrong_timecode() {
        let check = check_report(
            "Водитель поворачивает направо возле перекрёстка [reel_1@02:00].",
            &[source("reel_1")],
        );
        assert!(check.rejected[0].1.contains("таймкод"));
    }

    #[test]
    fn report_rejects_a_missing_citation() {
        let check = check_report(
            "Водитель поворачивает направо возле перекрёстка.",
            &[source("reel_1")],
        );
        assert!(check.rejected[0].1.contains("нет ссылки"));
    }

    #[test]
    fn report_rejects_unrelated_words() {
        let check = check_report(
            "Космические спутники анализируют древние рукописи [reel_1@00:40].",
            &[source("reel_1")],
        );
        assert!(check.rejected[0].1.contains("совпадающих слов"));
    }

    #[test]
    fn report_lists_sources_not_cited_by_verified_statements() {
        let check = check_report(
            "Водитель поворачивает направо возле перекрёстка [reel_1@00:40].",
            &[source("reel_1"), source("reel_2")],
        );
        assert_eq!(check.uncited_sources, vec!["reel_2"]);
    }

    #[test]
    fn block_and_page_citations_match_unit_positions() {
        let block = Source {
            id: "notes_file_2".to_string(),
            url: "file:notes.txt".to_string(),
            unit: Unit::Block,
            duration: 1,
            segments: vec![Segment {
                start: 0,
                end: 0,
                text: "Главное утверждение подтверждено текстовым источником.".to_string(),
            }],
        };
        assert!(verify_statement(
            "Главное утверждение подтверждено текстовым источником [notes_file_2@b0].",
            &[block]
        )
        .is_ok());

        let page = Source {
            id: "book_2024".to_string(),
            url: "file:book.pdf".to_string(),
            unit: Unit::Page,
            duration: 1,
            segments: vec![Segment {
                start: 1,
                end: 1,
                text: "На первой странице содержится важный научный вывод.".to_string(),
            }],
        };
        assert!(verify_statement(
            "На первой странице содержится важный научный вывод [book_2024@p1].",
            &[page]
        )
        .is_ok());
    }

    #[test]
    fn hour_minute_second_citation_is_supported() {
        let source = Source {
            id: "lecture_1".to_string(),
            url: "https://example.test/lecture".to_string(),
            unit: Unit::Sec,
            duration: 3700,
            segments: vec![Segment {
                start: 3600,
                end: 3700,
                text: "Лектор объясняет важный научный вывод.".to_string(),
            }],
        };
        assert!(verify_statement(
            "Лектор объясняет важный научный вывод [lecture_1@1:00:00].",
            &[source]
        )
        .is_ok());
    }
}
