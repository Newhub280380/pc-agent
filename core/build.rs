// Сборка C++-слоя прямо из cargo: одна команда `cargo build` собирает всё.
//
// Зачем cc вместо CMake: у нас 6 файлов и один таргет — CMake добавил бы
// внешнюю зависимость сборки без выгоды. CMakeLists всё же есть в native/
// для тех, кто хочет собирать библиотеку отдельно (например, под тесты).
fn main() {
    let target_os_for_router = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    embed_router(&target_os_for_router);
    println!("cargo:rerun-if-changed=../native/src");
    println!("cargo:rerun-if-changed=../native/include/agent_native.h");

    // Смотрим на ЦЕЛЬ, а не на хост: сам build.rs всегда собирается под хост,
    // поэтому cfg!(target_os) здесь врёт при кросс-компиляции и раньше давал
    // 30 «undefined reference to an_*» вместо понятной ошибки.
    let target_os = target_os_for_router;
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" {
        // На не-Windows C++-слоя нет: под Linux «руки и глаза» сделаны
        // утилитами рабочего стола (xdotool/scrot/tesseract), см. platform/linux_impl.rs.
        println!("cargo:warning=C++-слой пропущен: цель не Windows");
        return;
    }
    if target_env != "msvc" {
        // C++/WinRT (OCR, UI Automation) есть только в MSVC-заголовках: под
        // mingw эти интерфейсы отсутствуют, и линковка всё равно упадёт.
        panic!(
            "цель {target_os}-{target_env} не поддерживается: C++/WinRT-слой собирается только MSVC.\n\
             Собирай релиз так: cargo build --release --target x86_64-pc-windows-msvc (или просто cargo build --release на Windows)."
        );
    }

    let mut b = cc::Build::new();
    // C++20, а не 17: C++/WinRT под C++17 тянет <experimental/coroutine>,
    // который в свежих MSVC (VS2026) выдаёт static_assert «REMOVED SOON».
    // Это же снимает нужду в /await — корутины теперь штатные.
    b.cpp(true)
        .std("c++20")
        .include("../native/include")
        .file("../native/src/common.cpp")
        .file("../native/src/capture.cpp")
        .file("../native/src/input.cpp")
        .file("../native/src/vision.cpp")
        .file("../native/src/uia.cpp")
        .file("../native/src/env.cpp")
        .file("../native/src/ocr.cpp")
        .flag_if_supported("/EHsc")
        .define("UNICODE", None)
        .define("_UNICODE", None);
    b.compile("agent_native");

    link_windows_libs();
}

/// Вшиваем готовый бинарь Go-роутера в .exe ядра (требование «1 файл»).
/// Если Go-часть ещё не собрана — кладём пустышку, чтобы `cargo build`
/// не падал: ядро само подскажет пользователю собрать роутер.
/// Бинарь ищется под ЦЕЛЬ сборки: в рабочей копии рядом лежат и Windows-, и
/// Linux-роутер, а вшить не тот — значит получить агента, у которого сеть не
/// стартует вообще.
fn embed_router(target_os: &str) {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("router.bin");
    let candidates: [&str; 3] = if target_os == "windows" {
        [
            "../router/pcagent-router.exe",
            "../dist/pcagent-router.exe",
            "../router/pcagent-router",
        ]
    } else {
        [
            "../router/pcagent-router",
            "../dist/pcagent-router",
            "../router/pcagent-router.exe",
        ]
    };
    for c in candidates {
        println!("cargo:rerun-if-changed={c}");
        if let Ok(bytes) = std::fs::read(c) {
            std::fs::write(&out, bytes).unwrap();
            println!("cargo:warning=роутер вшит из {c}");
            return;
        }
    }
    std::fs::write(&out, b"").unwrap();
    println!("cargo:warning=pcagent-router.exe не найден — собери Go-часть перед релизной сборкой");
}

fn link_windows_libs() {
    for lib in [
        "user32",
        "gdi32",
        "shcore",
        "ole32",
        "oleaut32",
        "windowscodecs",
        "uiautomationcore",
        "shell32",
        "advapi32",
        "runtimeobject",
    ] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
