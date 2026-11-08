use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct Source {
    pub id: String,
    pub url: String,
    pub duration: u32,
    pub segments: Vec<Segment>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct Segment {
    pub start: u32,
    pub end: u32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportCheck {
    pub verified: Vec<String>,
    pub rejected: Vec<(String, String)>,
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
        let (citations, text, malformed) = extract_citations(&statement);
        if is_heading(&text) {
            continue;
        }
        let result = verify_statement(&citations, &text, malformed.as_deref(), sources);
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
    time: u32,
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
                if content.starts_with("reel_") {
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
    let rest = value.strip_prefix("reel_")?;
    let (number, time) = rest.split_once('@')?;
    if number.is_empty() || !number.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let (minutes, seconds) = time.split_once(':')?;
    if minutes.is_empty()
        || seconds.is_empty()
        || !minutes.chars().all(|ch| ch.is_ascii_digit())
        || !seconds.chars().all(|ch| ch.is_ascii_digit())
    {
        return None;
    }
    let minutes = minutes.parse::<u32>().ok()?;
    let seconds = seconds.parse::<u32>().ok()?;
    if seconds >= 60 {
        return None;
    }
    Some(Citation {
        source_id: format!("reel_{number}"),
        time: minutes.checked_mul(60)?.checked_add(seconds)?,
    })
}

fn is_heading(text: &str) -> bool {
    if !text.chars().any(char::is_alphabetic) {
        return true;
    }
    text.split_whitespace().count() <= 3 && !text.chars().any(|ch| ch.is_ascii_digit())
}

fn verify_statement(
    citations: &[Citation],
    text: &str,
    malformed: Option<&str>,
    sources: &[Source],
) -> Result<Vec<String>, String> {
    if let Some(citation) = malformed {
        return Err(format!("некорректная ссылка {citation}"));
    }
    if citations.is_empty() {
        return Err("нет ссылки вида [reel_N@mm:ss]".to_string());
    }

    let mut window = Vec::new();
    let mut source_ids = Vec::new();
    for citation in citations {
        let source = sources
            .iter()
            .find(|source| source.id == citation.source_id)
            .ok_or_else(|| format!("источник {} отсутствует", citation.source_id))?;
        let segment_index = source
            .segments
            .iter()
            .position(|segment| segment.start <= citation.time && citation.time <= segment.end)
            .ok_or_else(|| {
                format!(
                    "таймкод {} не попадает в сегмент источника {}",
                    timecode(citation.time),
                    citation.source_id
                )
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
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str) -> Source {
        Source {
            id: id.to_string(),
            url: format!("https://example.test/{id}"),
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
}
