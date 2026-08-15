// Сборка C++-слоя прямо из cargo: одна команда `cargo build` собирает всё.
//
// Зачем cc вместо CMake: у нас 6 файлов и один таргет — CMake добавил бы
// внешнюю зависимость сборки без выгоды. CMakeLists всё же есть в native/
// для тех, кто хочет собирать библиотеку отдельно (например, под тесты).
fn main() {
    embed_router();
    println!("cargo:rerun-if-changed=../native/src");
    println!("cargo:rerun-if-changed=../native/include/agent_native.h");

    if !cfg!(target_os = "windows") {
        // На не-Windows собирается только ядро (для проверки логики и CI).
        // Реальные «руки и глаза» существуют только под Windows.
        println!("cargo:warning=native-слой пропущен: цель не Windows (stub-режим)");
        return;
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
fn embed_router() {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("router.bin");
    let candidates = [
        "../router/pcagent-router.exe",
        "../dist/pcagent-router.exe",
        "../router/pcagent-router",
    ];
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
