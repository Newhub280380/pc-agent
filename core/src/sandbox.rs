//! Песочница для внешних процессов: таймаут, лимит вывода, чёрный список.
//!
//! Зачем: единственный способ, которым агент может «уронить систему», — это
//! запуск чужого процесса (launch_app, adb). LLM получает текст с экрана, а
//! значит через prompt injection ей можно подсунуть
//! `powershell -enc <base64>` или `vssadmin delete shadows`. Закрытый enum
//! действий защищает от произвольного шелла, но не от произвольного EXE.
//!
//! Что делаем:
//!   1. чёрный список программ, которые ломают систему необратимо;
//!   2. отказ от кодированных/скрытых команд интерпретаторов;
//!   3. жёсткий таймаут с убийством процесса — «зависший установщик» не
//!      останавливает агента навсегда;
//!   4. лимит на объём вывода — вывод уходит в промпт, а токены платные.
//!
//! Альтернативы: Job Object c ограничением памяти/CPU (Windows) или AppContainer.
//! Это правильнее и запланировано в v2; здесь — переносимый минимум, который
//! работает и тестируется на любой ОС.

use anyhow::{bail, Result};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Программы, которые агент не запускает никогда: их эффект необратим и
/// не наблюдаем на экране, то есть его нельзя ни проверить, ни откатить.
const DENY: &[&str] = &[
    "format",
    "diskpart",
    "vssadmin",
    "bcdedit",
    "cipher",
    "wbadmin",
    "reagentc",
    "mshta",
    "rundll32",
    "regsvr32",
    "bitsadmin",
    "certutil",
    "wmic",
    "netsh",
    "sc",
    "schtasks",
    "takeown",
    "icacls",
    "shutdown",
];

/// Интерпретаторы: разрешены только без «скрытых» флагов, потому что
/// `-EncodedCommand`/`-w hidden` — это ровно способ спрятать полезную нагрузку.
const SHELLS: &[&str] = &["powershell", "pwsh", "cmd", "wscript", "cscript"];

const HIDDEN_FLAGS: &[&str] = &[
    "-enc",
    "-encodedcommand",
    "-e ",
    "-w hidden",
    "-windowstyle hidden",
    "-nop",
    "-noprofile",
    "-executionpolicy bypass",
    "-ep bypass",
    "frombase64string",
    "iex",
    "invoke-expression",
    "downloadstring",
];

#[derive(Debug, Clone)]
pub struct Limits {
    pub timeout: Duration,
    pub max_output: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // 60 c хватает любой adb-команде и не даёт агенту зависнуть на час.
            timeout: Duration::from_secs(60),
            max_output: 64 * 1024,
        }
    }
}

#[derive(Debug)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Имя программы без пути и расширения — по нему и проверяем списки.
fn base_name(program: &str) -> String {
    program
        .trim()
        .trim_matches('"')
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("")
        .to_lowercase()
        .trim_end_matches(".exe")
        .trim_end_matches(".com")
        .to_string()
}

/// Разрешено ли вообще запускать это. Ошибка — понятная человеку: агент
/// покажет её в логе и попросит подтверждение или другой путь.
pub fn check_program(program: &str, args: &str) -> Result<()> {
    let p = program.trim();
    if p.is_empty() {
        bail!("пустой путь к программе");
    }
    if p.chars().any(|c| c.is_control()) {
        bail!("в пути к программе управляющие символы");
    }
    let name = base_name(p);
    if name.is_empty() {
        bail!("не могу определить имя программы в «{p}»");
    }
    if DENY.contains(&name.as_str()) {
        bail!("{name} запрещён: необратимо меняет систему и не проверяется по экрану");
    }
    let low_args = args.to_lowercase();
    if SHELLS.contains(&name.as_str()) {
        if let Some(flag) = HIDDEN_FLAGS.iter().find(|f| low_args.contains(**f)) {
            bail!("{name} со скрытой/кодированной командой ({flag}) запрещён");
        }
    }
    // Аргументы тоже проверяем: `explorer.exe C:\...\vssadmin.exe` — обход.
    for tok in low_args.split_whitespace() {
        let n = base_name(tok);
        if DENY.contains(&n.as_str()) {
            bail!("в аргументах запрещённая программа: {n}");
        }
    }
    Ok(())
}

/// Запуск с таймаутом и лимитом вывода. Не паникует ни при каком исходе:
/// худший случай — процесс убит и возвращена ошибка.
pub fn run(program: &str, args: &[String], limits: &Limits) -> Result<Output> {
    check_program(program, &args.join(" "))?;
    let mut child: Child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Читаем в отдельных потоках: полный конвейер (pipe) блокирует процесс, и
    // «таймаут» без чтения превратился бы в вечное ожидание на 4 КБ вывода.
    let max = limits.max_output;
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let t_out = std::thread::spawn(move || read_capped(&mut out_pipe, max));
    let t_err = std::thread::spawn(move || read_capped(&mut err_pipe, max));

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if started.elapsed() >= limits.timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                bail!("не могу дождаться процесса {program}: {e}");
            }
        }
    };

    // Паника внутри читающего потока не должна ронять агента.
    let (stdout, cut1) = t_out.join().unwrap_or_else(|_| (String::new(), false));
    let (stderr, cut2) = t_err.join().unwrap_or_else(|_| (String::new(), false));

    match status {
        Some(st) => Ok(Output {
            code: st.code(),
            stdout,
            stderr,
            truncated: cut1 || cut2,
        }),
        None => bail!(
            "{program} не завершился за {} с и был остановлен",
            limits.timeout.as_secs()
        ),
    }
}

fn read_capped<R: Read>(src: &mut Option<R>, max: usize) -> (String, bool) {
    let Some(r) = src.as_mut() else {
        return (String::new(), false);
    };
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let mut truncated = false;
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < max {
                    let take = n.min(max - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (String::from_utf8_lossy(&buf).to_string(), truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_programs_are_blocked() {
        assert!(check_program("vssadmin.exe", "delete shadows /all").is_err());
        assert!(check_program(r"C:\Windows\System32\diskpart.exe", "").is_err());
        assert!(check_program("shutdown", "/s /t 0").is_err());
        assert!(check_program("notepad.exe", "note.txt").is_ok());
    }

    #[test]
    fn hidden_powershell_payload_is_blocked() {
        assert!(check_program("powershell.exe", "-enc SQBFAFgA").is_err());
        assert!(
            check_program("powershell", "-w hidden -c iex (New-Object Net.WebClient)").is_err()
        );
        // Обычный вызов не запрещаем: агенту иногда нужен скрипт пользователя.
        assert!(check_program("powershell", "-File C:\\tools\\report.ps1").is_ok());
    }

    #[test]
    fn denied_program_in_args_is_blocked() {
        assert!(check_program("explorer.exe", r"C:\Windows\System32\format.com").is_err());
        assert!(check_program("", "").is_err());
        assert!(check_program("note\u{0}pad", "").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_hanging_process() {
        let lim = Limits {
            timeout: Duration::from_millis(300),
            max_output: 1024,
        };
        let err = run("/bin/sleep", &["30".to_string()], &lim).expect_err("должен быть таймаут");
        assert!(err.to_string().contains("не завершился"));
    }

    #[cfg(unix)]
    #[test]
    fn output_is_captured_and_capped() {
        let lim = Limits {
            timeout: Duration::from_secs(10),
            max_output: 16,
        };
        let out = run(
            "/bin/sh",
            &["-c".into(), "printf 'x%.0s' $(seq 1 1000)".into()],
            &lim,
        )
        .expect("процесс должен отработать");
        assert!(out.ok());
        assert_eq!(out.stdout.len(), 16);
        assert!(out.truncated);
    }
}
