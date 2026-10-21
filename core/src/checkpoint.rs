//! Транзакционный чекпоинт перед шагом.
//!
//! Снимок — это сразу четыре вещи: файлы, которые задача уже трогала,
//! активное окно, граф с трассой и полная копия памяти. Манифест пишется
//! последним. Пока его нет, снимок для отката не существует.
//!
//! Откат либо возвращает всё это вместе, либо не меняет живое состояние.
//! До первой записи мы читаем текущие файлы и копию памяти в сторону.
//! Если посреди записи что-то падает, эти копии кладутся обратно.
//! «Починили граф, а файл оставили новый» отсюда не получается.

use crate::agent::graph::TaskGraph;
use crate::agent::react::Step;
use crate::memory::Memory;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Больше этого файл в снимок не берём: лучше отказаться от шага,
/// чем записать «чекпоинт», который нельзя вернуть.
const MAX_FILE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileImage {
    pub path: String,
    pub present: bool,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub task_id: String,
    pub task: String,
    pub step: u32,
    pub graph: TaskGraph,
    pub observed: BTreeMap<String, bool>,
    pub window_id: u64,
    pub window_title: String,
    pub files: Vec<FileImage>,
    pub criteria: Vec<String>,
    pub trace: Vec<Step>,
    pub touched: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    snapshot: Snapshot,
    memory: String,
    file_digest: Vec<String>,
}

pub fn default_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("PCAgent")
        .join("checkpoints")
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Снять снимок. Любая ошибка чтения стирает черновик и оставляет
    /// предыдущий хороший снимок как был.
    pub fn save(&self, snap: &Snapshot, mem: &Memory) -> Result<PathBuf> {
        for f in &snap.files {
            if f.bytes.len() > MAX_FILE {
                bail!(
                    "файл {} больше {} байт, чекпоинт неполный",
                    f.path,
                    MAX_FILE
                );
            }
        }
        let dir = self.step_dir(&snap.task_id, snap.step);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        let draft = self
            .root
            .join(&snap.task_id)
            .join(format!(".draft-{}", snap.step));
        if draft.exists() {
            fs::remove_dir_all(&draft)?;
        }
        fs::create_dir_all(&draft)?;
        let save_result = (|| {
            let memory_path = draft.join("memory.db");
            mem.backup_to(&memory_path)?;
            let mut digests = Vec::new();
            for (i, file) in snap.files.iter().enumerate() {
                let blob = draft.join(format!("file-{i}"));
                fs::write(&blob, &file.bytes)
                    .with_context(|| format!("не записал {}", file.path))?;
                digests.push(digest(&file.bytes));
            }
            let manifest = Manifest {
                snapshot: snap.clone(),
                memory: "memory.db".into(),
                file_digest: digests,
            };
            let raw = serde_json::to_vec_pretty(&manifest)?;
            fs::write(draft.join("manifest.json"), raw)?;
            Ok::<(), anyhow::Error>(())
        })();
        if let Err(e) = save_result {
            let _ = fs::remove_dir_all(&draft);
            return Err(e);
        }
        fs::create_dir_all(self.root.join(&snap.task_id))?;
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::rename(&draft, &dir).context("не удалось опубликовать чекпоинт")?;
        self.prune(&snap.task_id, 5)?;
        Ok(dir)
    }

    pub fn load_latest(&self, task_id: &str) -> Result<Option<Snapshot>> {
        let Some(dir) = self.latest_dir(task_id)? else {
            return Ok(None);
        };
        self.load_dir(&dir).map(Some)
    }

    /// Прочитать последний снимок и применить его. Если снимок битый,
    /// живое состояние не трогаем.
    pub fn restore_latest(
        &self,
        task_id: &str,
        mem: &mut Memory,
        focus: &mut dyn FnMut(u64, &str) -> Result<(), String>,
    ) -> Result<Snapshot> {
        let dir = self
            .latest_dir(task_id)?
            .ok_or_else(|| anyhow::anyhow!("для {task_id} нет целого чекпоинта"))?;
        let snap = self.load_dir(&dir)?;
        apply_all(&snap, &dir.join("memory.db"), mem, focus)?;
        Ok(snap)
    }

    fn load_dir(&self, dir: &Path) -> Result<Snapshot> {
        let raw = fs::read(dir.join("manifest.json"))
            .with_context(|| format!("нет манифеста в {}", dir.display()))?;
        let manifest: Manifest = serde_json::from_slice(&raw)
            .with_context(|| format!("битый манифест в {}", dir.display()))?;
        if manifest.file_digest.len() != manifest.snapshot.files.len() {
            bail!("манифест {} не сходится с числом файлов", dir.display());
        }
        let mut files = manifest.snapshot.files.clone();
        for (i, file) in files.iter_mut().enumerate() {
            let blob = dir.join(format!("file-{i}"));
            let bytes =
                fs::read(&blob).with_context(|| format!("в чекпоинте нет {}", file.path))?;
            if digest(&bytes) != manifest.file_digest[i] {
                bail!("чекпоинт повреждён: {} не сходится с манифестом", file.path);
            }
            file.bytes = bytes;
        }
        if !dir.join(&manifest.memory).is_file() {
            bail!("в чекпоинте {} нет копии памяти", dir.display());
        }
        let mut snap = manifest.snapshot;
        snap.files = files;
        Ok(snap)
    }

    fn step_dir(&self, task_id: &str, step: u32) -> PathBuf {
        self.root.join(task_id).join(format!("{step:06}"))
    }

    fn latest_dir(&self, task_id: &str) -> Result<Option<PathBuf>> {
        let parent = self.root.join(task_id);
        if !parent.is_dir() {
            return Ok(None);
        }
        let mut best: Option<(u32, PathBuf)> = None;
        for ent in fs::read_dir(&parent)? {
            let ent = ent?;
            let name = ent.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let Ok(step) = name.parse::<u32>() else {
                continue;
            };
            if !ent.path().join("manifest.json").is_file() {
                continue;
            }
            if best.as_ref().map(|(s, _)| step > *s).unwrap_or(true) {
                best = Some((step, ent.path()));
            }
        }
        Ok(best.map(|(_, p)| p))
    }

    fn prune(&self, task_id: &str, keep: usize) -> Result<()> {
        let parent = self.root.join(task_id);
        let mut steps = Vec::new();
        for ent in fs::read_dir(&parent)? {
            let ent = ent?;
            let name = ent.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Ok(step) = name.parse::<u32>() {
                if ent.path().join("manifest.json").is_file() {
                    steps.push((step, ent.path()));
                }
            }
        }
        steps.sort_by_key(|(s, _)| *s);
        let drop_n = steps.len().saturating_sub(keep);
        for (_, path) in steps.into_iter().take(drop_n) {
            fs::remove_dir_all(path)?;
        }
        Ok(())
    }
}

/// Сначала чтение живого состояния в сторону, потом запись снимка.
/// Любая ошибка после первой записи возвращает и файлы, и память.
fn apply_all(
    snap: &Snapshot,
    memory_db: &Path,
    mem: &mut Memory,
    focus: &mut dyn FnMut(u64, &str) -> Result<(), String>,
) -> Result<()> {
    if !memory_db.is_file() {
        bail!("в чекпоинте нет копии памяти");
    }
    let undo = read_current(&snap.files)?;
    let memory_undo = tempfile_db()?;
    mem.backup_to(&memory_undo)
        .context("не смог отложить текущую память перед откатом")?;

    if let Err(e) = write_files(&snap.files) {
        let _ = write_files(&undo);
        bail!("откат файлов не завершён, вернул как было: {e}");
    }
    if let Err(e) = mem.restore_from(memory_db) {
        let _ = write_files(&undo);
        if let Err(undo_err) = mem.restore_from(&memory_undo) {
            bail!("память не откатилась и запасная копия тоже: {e}; {undo_err}");
        }
        bail!("память не откатилась, файлы возвращены: {e}");
    }
    if snap.window_id != 0 {
        if let Err(e) = focus(snap.window_id, &snap.window_title) {
            let _ = write_files(&undo);
            if let Err(undo_err) = mem.restore_from(&memory_undo) {
                bail!("окно не вернулось и память тоже: {e}; {undo_err}");
            }
            bail!("окно не вернулось, файлы и память возвращены: {e}");
        }
    }
    let _ = fs::remove_file(&memory_undo);
    Ok(())
}

fn read_current(files: &[FileImage]) -> Result<Vec<FileImage>> {
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let path = Path::new(&f.path);
        match fs::read(path) {
            Ok(bytes) => out.push(FileImage {
                path: f.path.clone(),
                present: true,
                bytes,
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => out.push(FileImage {
                path: f.path.clone(),
                present: false,
                bytes: Vec::new(),
            }),
            Err(e) => bail!("не прочитал {} перед откатом, ничего не меняю: {e}", f.path),
        }
    }
    Ok(out)
}

fn write_files(files: &[FileImage]) -> Result<()> {
    for (i, f) in files.iter().enumerate() {
        if let Err(e) = write_one(f) {
            for prev in files.iter().take(i).rev() {
                // Сюда попадают уже новые байты. Вызывающий при ошибке
                // пишет отдельный undo. Здесь только пытаемся не оставить
                // обрезанный файл: если запись не удалась, удаляем огрызок
                // когда файла быть не должно.
                if !prev.present {
                    let _ = fs::remove_file(&prev.path);
                }
            }
            bail!("не записал {}: {e}", f.path);
        }
    }
    Ok(())
}

fn write_one(f: &FileImage) -> Result<()> {
    let path = Path::new(&f.path);
    if !f.present {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &f.bytes)?;
        Ok(())
    }
}

fn tempfile_db() -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!(
        "pcagent-undo-{}-{}.db",
        std::process::id(),
        rand::random::<u64>()
    ));
    Ok(path)
}

pub fn digest(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}:{}", bytes.len())
}

/// Снять содержимое путей. Отсутствующий файл — это тоже факт снимка:
/// после шага его появление будет стёрто откатом.
pub fn read_files(paths: &[PathBuf]) -> Result<Vec<FileImage>> {
    let mut out = Vec::new();
    for path in paths {
        if path.as_os_str().is_empty() {
            continue;
        }
        match fs::read(path) {
            Ok(bytes) => {
                if bytes.len() > MAX_FILE {
                    bail!("{} слишком большой для чекпоинта", path.display());
                }
                out.push(FileImage {
                    path: path.display().to_string(),
                    present: true,
                    bytes,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => out.push(FileImage {
                path: path.display().to_string(),
                present: false,
                bytes: Vec::new(),
            }),
            Err(e) => bail!("не прочитал {}: {e}", path.display()),
        }
    }
    Ok(out)
}

/// Пути, которые действие может изменить. Каталоги не разворачиваем:
/// в снимок попадает сам файл, который агент назвал.
pub fn paths_of_action(action_path: &str) -> Vec<PathBuf> {
    let p = action_path.trim();
    if p.is_empty() {
        return Vec::new();
    }
    if p.contains('/') || p.contains('\\') || p.contains('.') {
        vec![PathBuf::from(p)]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::graph::TaskGraph;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(name: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("pcagent-ckpt-{name}-{n}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample(task_id: &str, step: u32, files: Vec<FileImage>) -> Snapshot {
        let mut g = TaskGraph::linear("цель", &[("шаг".into(), "видно «Готово»".into())]);
        g.validate().unwrap();
        let touched = files.iter().map(|f| f.path.clone()).collect();
        Snapshot {
            task_id: task_id.into(),
            task: "цель".into(),
            step,
            graph: g,
            observed: BTreeMap::new(),
            window_id: 7,
            window_title: "блокнот".into(),
            files,
            criteria: vec!["«Готово»".into()],
            trace: Vec::new(),
            touched,
        }
    }

    #[test]
    fn rollback_restores_files_memory_graph_and_window_together() {
        let dir = scratch("ok");
        let store = Store::open(dir.join("store")).unwrap();
        let file_a = dir.join("a.txt");
        let file_b = dir.join("b.txt");
        fs::write(&file_a, b"a1").unwrap();
        fs::write(&file_b, b"b1").unwrap();
        let db = dir.join("memory.db");
        let mem = Memory::open(&db).unwrap();
        mem.remember("app:test", "fact", "было", "первое", "", 0.5)
            .unwrap();
        let images = read_files(&[file_a.clone(), file_b.clone()]).unwrap();
        let snap = sample("t1", 1, images);
        store.save(&snap, &mem).unwrap();

        fs::write(&file_a, b"CHANGED").unwrap();
        fs::remove_file(&file_b).unwrap();
        let extra = dir.join("c.txt");
        fs::write(&extra, b"new").unwrap();
        mem.remember("app:test", "fact", "стало", "второе", "", 0.5)
            .unwrap();

        let mut focus = None;
        let mut mem = mem;
        let back = store
            .restore_latest("t1", &mut mem, &mut |id, title| {
                focus = Some((id, title.to_string()));
                Ok(())
            })
            .unwrap();
        assert_eq!(fs::read(&file_a).unwrap(), b"a1");
        assert_eq!(fs::read(&file_b).unwrap(), b"b1");
        assert_eq!(back.graph.goal, "цель");
        assert_eq!(back.window_title, "блокнот");
        assert_eq!(focus, Some((7, "блокнот".into())));
        let items = mem.recall("app:test", "было", 5).unwrap();
        assert_eq!(
            items.len(),
            1,
            "память должна вернуться целиком, без второй записи"
        );
        assert!(mem.recall("app:test", "стало", 5).unwrap().is_empty());
    }

    #[test]
    fn broken_checkpoint_does_not_touch_live_state() {
        let dir = scratch("bad");
        let store = Store::open(dir.join("store")).unwrap();
        let file_a = dir.join("a.txt");
        fs::write(&file_a, b"live").unwrap();
        let db = dir.join("memory.db");
        let mem = Memory::open(&db).unwrap();
        mem.remember("app:test", "fact", "жив", "да", "", 0.4)
            .unwrap();
        let images = read_files(&[file_a.clone()]).unwrap();
        let snap = sample("t2", 1, images);
        let saved = store.save(&snap, &mem).unwrap();
        fs::remove_file(saved.join("memory.db")).unwrap();

        fs::write(&file_a, b"after").unwrap();
        mem.remember("app:test", "fact", "ещё", "нет", "", 0.4)
            .unwrap();
        let mut mem = mem;
        let err = store.restore_latest("t2", &mut mem, &mut |_, _| Ok(()));
        assert!(err.is_err(), "битый снимок не имеет права примениться");
        assert_eq!(fs::read(&file_a).unwrap(), b"after");
        assert_eq!(mem.recall("app:test", "ещё", 5).unwrap().len(), 1);
    }

    #[test]
    fn focus_failure_rolls_files_and_memory_back() {
        let dir = scratch("focus");
        let store = Store::open(dir.join("store")).unwrap();
        let file_a = dir.join("a.txt");
        fs::write(&file_a, b"was").unwrap();
        let db = dir.join("memory.db");
        let mem = Memory::open(&db).unwrap();
        mem.remember("app:test", "fact", "alphaone", "1", "", 0.4)
            .unwrap();
        let snap = sample("t3", 1, read_files(&[file_a.clone()]).unwrap());
        store.save(&snap, &mem).unwrap();
        fs::write(&file_a, b"now").unwrap();
        mem.remember("app:test", "fact", "omegatwo", "2", "", 0.4)
            .unwrap();
        let mut mem = mem;
        let err = store.restore_latest("t3", &mut mem, &mut |_, _| Err("окна нет".into()));
        assert!(err.is_err());
        assert_eq!(
            fs::read(&file_a).unwrap(),
            b"now",
            "после неудачного фокуса файл обязан остаться живым"
        );
        assert_eq!(mem.recall("app:test", "omegatwo", 5).unwrap().len(), 1);
        assert_eq!(mem.recall("app:test", "alphaone", 5).unwrap().len(), 1);
    }
}
