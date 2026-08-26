#!/usr/bin/env bash
# Установщик PC Agent для Ubuntu/Debian: один файл, двойной клик или
#   bash install-linux.sh
# Ставит зависимости, качает агента из GitHub Releases, спрашивает ключ,
# создаёт конфиг и ярлык в меню приложений и на рабочем столе.
set -u

REPO="Newhub280380/pc-agent"
DIR="${XDG_DATA_HOME:-$HOME/.local/share}/PCAgent"
BIN="$DIR/pcagent"
CFG="$DIR/config.json"

say() { printf '\n== %s\n' "$1"; }
die() { printf '\nОШИБКА: %s\n' "$1" >&2; read -r -p "Enter для выхода..." _ || true; exit 1; }

say "Проверяю зависимости"
need=()
for t in curl xdotool scrot tesseract xclip xprintidle; do
  command -v "$t" >/dev/null 2>&1 || need+=("$t")
done
if [ "${#need[@]}" -gt 0 ]; then
  pkgs=""
  for t in "${need[@]}"; do
    case "$t" in
      tesseract) pkgs="$pkgs tesseract-ocr tesseract-ocr-rus" ;;
      *) pkgs="$pkgs $t" ;;
    esac
  done
  echo "Нужно установить:$pkgs (потребуется ваш пароль sudo)"
  sudo apt-get update -qq || die "не удалось обновить список пакетов"
  # shellcheck disable=SC2086
  sudo apt-get install -y $pkgs || die "не удалось установить:$pkgs"
fi

say "Ставлю агента"
mkdir -p "$DIR" || die "не могу создать $DIR"
here="$(cd "$(dirname "$0")" && pwd)"
local_bin=""
for f in "$here/pcagent-linux-x64" "$here/pcagent"; do
  [ -f "$f" ] && local_bin="$f" && break
done
if [ -n "$local_bin" ]; then
  # Файл рядом со скриптом: так работает и с приватным репозиторием, где
  # релизы без авторизации не скачиваются.
  cp "$local_bin" "$BIN.tmp" || die "не могу скопировать $local_bin"
else
  url="https://github.com/$REPO/releases/latest/download/pcagent-linux-x64"
  curl -fL --retry 3 -o "$BIN.tmp" "$url" \
    || die "не удалось скачать агента ($url). Положите файл pcagent-linux-x64 рядом с этим скриптом и запустите снова"
fi
# Рядом с бинарником может лежать pcagent-linux-x64.sha256 — если он есть,
# скачанный файл проверяется, иначе подмену на хостинге никто не заметит.
sums=""
if [ -n "$local_bin" ] && [ -f "$local_bin.sha256" ]; then
  sums=$(cat "$local_bin.sha256")
else
  sums=$(curl -fsSL "https://github.com/$REPO/releases/latest/download/pcagent-linux-x64.sha256" 2>/dev/null || true)
fi
if [ -n "$sums" ]; then
  got=$(sha256sum "$BIN.tmp" | cut -d' ' -f1)
  case "$sums" in
    *"$got"*) ;;
    *) rm -f "$BIN.tmp"; die "контрольная сумма не совпала — файл повреждён или подменён" ;;
  esac
fi
mv "$BIN.tmp" "$BIN"
chmod +x "$BIN"

if [ ! -s "$CFG" ]; then
  say "Ключ LLM"
  echo "Вставьте ключ NVIDIA (начинается на nvapi-) и нажмите Enter."
  echo "Можно пропустить (просто Enter) — тогда агент запустится без «мозга»."
  read -r key
  if [ -n "$key" ]; then
    printf '{\n  "llm_provider": "nvidia",\n  "api_key": "%s"\n}\n' "$key" > "$CFG"
  else
    printf '{\n  "llm_provider": "kilo"\n}\n' > "$CFG"
  fi
  chmod 600 "$CFG"
fi

say "Ярлык"
entry="[Desktop Entry]
Type=Application
Name=PC Agent
Comment=Автономный агент за компьютером
Exec=$BIN
Terminal=false
Categories=Utility;"
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
mkdir -p "$apps"
printf '%s\n' "$entry" > "$apps/pcagent.desktop"
chmod +x "$apps/pcagent.desktop"
desk=$(xdg-user-dir DESKTOP 2>/dev/null || echo "$HOME/Desktop")
if [ -d "$desk" ]; then
  printf '%s\n' "$entry" > "$desk/pcagent.desktop"
  chmod +x "$desk/pcagent.desktop"
  gio set "$desk/pcagent.desktop" metadata::trusted true 2>/dev/null || true
fi

say "Запускаю"
setsid "$BIN" >/dev/null 2>&1 &
echo "Готово. Агент здесь: $BIN"
echo "Ярлык «PC Agent» в меню приложений и на рабочем столе."
