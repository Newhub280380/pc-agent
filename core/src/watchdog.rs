//! Сторож зависания и падения.
//!
//! На каждом шаге пишется пульс: id задачи, номер шага, время.
//! Если пульс старше порога, а задача всё ещё `running` — это зависание,
//! и цикл возвращается к последнему целому чекпоинту, а не продолжает
//! с половины шага. Если процесса уже нет (следующий запуск видит
//! `running` и чекпоинт) — та же точка входа, задача продолжается.

use crate::checkpoint::{Snapshot, Store};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watch {
    Healthy,
    Hung,
    Crashed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Beat {
    pub task_id: String,
    pub step: u32,
    pub unix_ms: u128,
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn write_beat(path: &Path, beat: &Beat) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("beat.tmp");
    std::fs::write(&tmp, serde_json::to_vec(beat)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn read_beat(path: &Path) -> Option<Beat> {
    let raw = std::fs::read(path).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// `running` — задача в базе ещё не закрыта. Нет пульса при живой задаче
/// значит процесс умер до первой отметки или файл не дописался.
pub fn assess(running: bool, beat: Option<&Beat>, now: u128, hang: Duration) -> Watch {
    if !running {
        return Watch::Healthy;
    }
    let Some(beat) = beat else {
        return Watch::Crashed;
    };
    let hang_ms = hang.as_millis();
    if now.saturating_sub(beat.unix_ms) > hang_ms {
        Watch::Hung
    } else {
        Watch::Healthy
    }
}

/// Какую брошенную задачу поднимать. Берём ту, у которой есть целый
/// чекпоинт и самый большой номер шага. Без снимка продолжать нечего:
/// это была бы новая задача под видом старой.
pub fn resume_candidate(
    store: &Store,
    running: &[(String, String)],
) -> Result<Option<(String, String, Snapshot)>> {
    let mut best: Option<(u32, String, String, Snapshot)> = None;
    for (id, task) in running {
        if let Some(snap) = store.load_latest(id)? {
            let take = best
                .as_ref()
                .map(|(step, _, _, _)| snap.step >= *step)
                .unwrap_or(true);
            if take {
                best = Some((snap.step, id.clone(), task.clone(), snap));
            }
        }
    }
    Ok(best.map(|(_, id, task, snap)| (id, task, snap)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::graph::TaskGraph;
    use crate::checkpoint::{FileImage, Snapshot, Store};
    use crate::memory::Memory;
    use std::collections::BTreeMap;

    #[test]
    fn stale_beat_is_a_hang_and_missing_beat_is_a_crash() {
        let beat = Beat {
            task_id: "t".into(),
            step: 3,
            unix_ms: 1_000,
        };
        assert_eq!(
            assess(true, Some(&beat), 1_000 + 5_000, Duration::from_secs(30)),
            Watch::Healthy
        );
        assert_eq!(
            assess(true, Some(&beat), 1_000 + 31_000, Duration::from_secs(30)),
            Watch::Hung
        );
        assert_eq!(
            assess(true, None, 5_000, Duration::from_secs(30)),
            Watch::Crashed
        );
        assert_eq!(
            assess(false, None, 5_000, Duration::from_secs(30)),
            Watch::Healthy
        );
    }

    #[test]
    fn resume_picks_the_checkpoint_not_a_task_without_one() {
        let dir = std::env::temp_dir().join(format!("pcagent-wd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(dir.join("ckpt")).unwrap();
        let db = dir.join("m.db");
        let mem = Memory::open(&db).unwrap();
        let mut graph = TaskGraph::linear("цель", &[("шаг".into(), "видно «Готово»".into())]);
        graph.validate().unwrap();
        let snap = Snapshot {
            task_id: "keep".into(),
            task: "поставить".into(),
            step: 4,
            graph,
            observed: BTreeMap::new(),
            window_id: 0,
            window_title: String::new(),
            files: Vec::<FileImage>::new(),
            criteria: vec!["«Готово»".into()],
            trace: Vec::new(),
            touched: Vec::new(),
        };
        store.save(&snap, &mem).unwrap();
        let running = vec![
            ("gone".into(), "без снимка".into()),
            ("keep".into(), "поставить".into()),
        ];
        let got = resume_candidate(&store, &running).unwrap().unwrap();
        assert_eq!(got.0, "keep");
        assert_eq!(got.2.step, 4);
    }
}
