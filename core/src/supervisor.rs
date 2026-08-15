//! Запуск и присмотр за Go-роутером.
//!
//! Требование «1 .exe»: бинарь роутера вшит в ядро через include_bytes! и
//! распаковывается в %APPDATA%\PCAgent при первом запуске. Пользователь
//! видит один файл; технически процессов два, и это осознанно —
//! падение сетевого слоя не убивает агента посреди задачи.
//!
//! Альтернатива (рассматривалась): собрать Go как c-archive и слинковать в
//! Rust. Тогда правда один процесс, но CGO ломает кросс-сборку, а паника в
//! Go-рантайме валит весь агент. Изоляция важнее.

use anyhow::{bail, Result};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Вшитый бинарь роутера. build.rs кладёт сюда либо реальный exe,
/// либо пустышку (если Go-часть ещё не собрана) — сборка не ломается.
static ROUTER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/router.bin"));

pub struct Router {
    child: Option<Child>,
}

impl Router {
    /// Распаковывает (если нужно) и запускает роутер. Уже запущенный
    /// экземпляр переиспользуется — второй запуск агента не поднимет дубль.
    pub fn start(exe_path: &Path, env_file: &Path, health_url: &str) -> Result<Self> {
        if ping(health_url) {
            log::info!("роутер уже запущен, переиспользую");
            return Ok(Self { child: None });
        }
        if ROUTER_BIN.len() < 1024 {
            bail!(
                "бинарь роутера не вшит в сборку. Собери Go-часть: scripts/build.ps1 (или positioning pcagent-router.exe рядом с агентом)"
            );
        }
        // Перезаписываем только если содержимое отличается: иначе антивирус
        // каждый запуск видит «новый файл» и лезет со сканом.
        let need_write = std::fs::read(exe_path)
            .map(|old| old != ROUTER_BIN)
            .unwrap_or(true);
        if need_write {
            std::fs::write(exe_path, ROUTER_BIN)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(exe_path, std::fs::Permissions::from_mode(0o755))?;
            }
        }

        let mut cmd = Command::new(exe_path);
        cmd.arg("-env")
            .arg(env_file)
            .current_dir(exe_path.parent().unwrap_or(Path::new(".")))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW); // без чёрного консольного окна
        }
        let child = cmd.spawn()?;

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if ping(health_url) {
                return Ok(Self { child: Some(child) });
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        bail!("роутер не ответил на /health за 10 секунд")
    }
}

impl Drop for Router {
    fn drop(&mut self) {
        if let Some(c) = &mut self.child {
            let _ = c.kill();
        }
    }
}

fn ping(health_url: &str) -> bool {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(700))
        .build()
        .get(health_url)
        .call()
        .is_ok()
}
