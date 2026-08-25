// Package config: загрузка .env и профилей провайдеров.
//
// Зачем: пользователь сам вставляет ключи от любых провайдеров, поэтому
// конфигурация должна быть данными, а не кодом. Ни один провайдер не
// зашит как "основной".
//
// Альтернативы:
//   - godotenv (внешняя зависимость) — отказались ради нулевых зависимостей;
//   - YAML/TOML-конфиг — удобнее для сложных сценариев, но требует парсера;
//     .env выбран потому, что это то, что просил пользователь.
package config

import (
	"bufio"
	"bytes"
	"encoding/binary"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"unicode/utf16"
)

// Provider — описание одного LLM-бэкенда.
type Provider struct {
	Name     string // логическое имя: openai, anthropic, gemini, deepseek, grok, qwen, openrouter, local
	Kind     string // протокол: "openai" | "anthropic" | "gemini"
	BaseURL  string
	APIKey   string
	Model    string
	Priority int  // меньше = выше приоритет при выборе
	Vision   bool // умеет ли принимать картинки (критично: агент шлёт скриншоты)
}

// Config — всё, что роутер читает из окружения.
type Config struct {
	Providers  []Provider
	Listen     string // адрес локального HTTP-сервера, только 127.0.0.1
	AuthToken  string // общий секрет между Rust-ядром и Go-роутером
	LogDir     string
	MaxRetries int
	TimeoutSec int
}

// providerSpec — таблица известных провайдеров.
// Зачем таблица: добавление нового провайдера = одна строка, а не новый код.
var providerSpecs = []struct {
	name        string
	kind        string
	envKey      string
	envModel    string
	envBase     string
	defaultBase string
	defModel    string
	vision      bool
}{
	{"openai", "openai", "OPENAI_API_KEY", "OPENAI_MODEL", "OPENAI_BASE_URL", "https://api.openai.com/v1", "gpt-4o", true},
	{"anthropic", "anthropic", "ANTHROPIC_API_KEY", "ANTHROPIC_MODEL", "ANTHROPIC_BASE_URL", "https://api.anthropic.com/v1", "claude-sonnet-4-20250514", true},
	{"gemini", "gemini", "GEMINI_API_KEY", "GEMINI_MODEL", "GEMINI_BASE_URL", "https://generativelanguage.googleapis.com/v1beta", "gemini-2.0-flash", true},
	{"deepseek", "openai", "DEEPSEEK_API_KEY", "DEEPSEEK_MODEL", "DEEPSEEK_BASE_URL", "https://api.deepseek.com/v1", "deepseek-chat", false},
	{"grok", "openai", "XAI_API_KEY", "XAI_MODEL", "XAI_BASE_URL", "https://api.x.ai/v1", "grok-2-vision-1212", true},
	{"qwen", "openai", "QWEN_API_KEY", "QWEN_MODEL", "QWEN_BASE_URL", "https://dashscope-intl.aliyuncs.com/compatible-mode/v1", "qwen-vl-max", true},
	{"openrouter", "openai", "OPENROUTER_API_KEY", "OPENROUTER_MODEL", "OPENROUTER_BASE_URL", "https://openrouter.ai/api/v1", "qwen/qwen2.5-vl-72b-instruct", true},
	{"youtoria", "openai", "YOUTORIA_API_KEY", "YOUTORIA_MODEL", "YOUTORIA_BASE_URL", "https://api.youtoria.ai/v1", "gpt-4o", true},
	// nvidia: NIM-каталог NVIDIA в OpenAI-совместимом режиме, vision-модель
	// llama-3.2-90b проверена на скриншотах.
	{"nvidia", "openai", "NVIDIA_API_KEY", "NVIDIA_MODEL", "NVIDIA_BASE_URL", "https://integrate.api.nvidia.com/v1", "meta/llama-3.2-90b-vision-instruct", true},
	// kilo: OpenAI-совместимый шлюз Kilo. Бесплатные модели отвечают без ключа,
	// платные требуют его, поэтому провайдер активен и с пустым KILO_API_KEY.
	{"kilo", "openai", "KILO_API_KEY", "KILO_MODEL", "KILO_BASE_URL", "https://api.kilo.ai/api/gateway", "kilo-auto/frontier", true},
	// local: llama.cpp / LM Studio / Ollama в OpenAI-совместимом режиме.
	// Ключ не обязателен, поэтому активируется наличием LOCAL_BASE_URL.
	{"local", "openai", "LOCAL_API_KEY", "LOCAL_MODEL", "LOCAL_BASE_URL", "", "qwen2.5-vl-7b", true},
}

// keyless — провайдеры, которые отвечают без API-ключа: локальные серверы и
// бесплатный уровень Kilo. Остальных без ключа включать бессмысленно: первый
// же запрос вернул бы 401.
func keyless(name string) bool {
	return name == "local" || name == "kilo"
}

// defaultModel: без ключа шлюз Kilo отдаёт только бесплатные модели — просить
// у него платный frontier значит гарантированно получить 401.
func defaultModel(name, key, def string) string {
	if name == "kilo" && key == "" {
		return "kilo-auto/free"
	}
	return def
}

// visionOK: бесплатный уровень Kilo картинки не принимает (проверка шлюза дала
// 400 на image_url и 429 на лимитах), поэтому шаги со скриншотом должны уходить
// другому провайдеру, а не падать на явной ошибке.
func visionOK(name, key string, def bool) bool {
	if name == "kilo" && key == "" {
		return false
	}
	return def
}

// checkLoopback не даёт роутеру с чужими API-ключами уехать в локальную сеть.
// Опечатка в ROUTER_LISTEN ("0.0.0.0:8713") иначе превращает ноутбук в
// открытый LLM-прокси для всех соседей по Wi-Fi.
func checkLoopback(addr string) error {
	host, _, err := net.SplitHostPort(addr)
	if err != nil {
		return fmt.Errorf("ROUTER_LISTEN должен быть вида host:port: %w", err)
	}
	if host == "localhost" {
		return nil
	}
	ip := net.ParseIP(host)
	if ip == nil || !ip.IsLoopback() {
		return fmt.Errorf("ROUTER_LISTEN=%q: разрешён только loopback (127.0.0.1/::1)", addr)
	}
	return nil
}

// Load читает .env (если есть) и переменные процесса.
// Переменные процесса имеют приоритет над файлом — так удобнее отлаживать;
// ядро через них же прокидывает содержимое config.json.
func Load(envPath string) (*Config, error) {
	fileVals, err := parseDotenv(envPath)
	if err != nil {
		return nil, err
	}
	get := func(k string) string {
		if v, ok := os.LookupEnv(k); ok && strings.TrimSpace(v) != "" {
			return strings.TrimSpace(v)
		}
		return strings.TrimSpace(fileVals[k])
	}
	if len(fileVals) == 0 && fileExists(envPath) {
		// Файл есть, переменных нет: обычно Легаси-кодировка из Блокнота.
		fmt.Fprintf(os.Stderr, "%s: переменные не найдены — сохрани файл как UTF-8 в виде КЛЮЧ=значение\n", envPath)
	}

	listen := firstNonEmpty(get("ROUTER_LISTEN"), "127.0.0.1:8713")
	if err := checkLoopback(listen); err != nil {
		return nil, err
	}

	cfg := &Config{
		Listen:     listen,
		AuthToken:  get("ROUTER_TOKEN"),
		LogDir:     firstNonEmpty(get("LOG_DIR"), filepath.Join(".", "logs")),
		MaxRetries: atoiDefault(get("LLM_MAX_RETRIES"), 3),
		TimeoutSec: atoiDefault(get("LLM_TIMEOUT_SEC"), 120),
	}

	// Порядок фолбэка задаётся LLM_PRIORITY="anthropic,openai,openrouter".
	prio := map[string]int{}
	for i, n := range splitList(get("LLM_PRIORITY")) {
		prio[n] = i
	}
	// Кого человек назвал сам — по имени провайдера или через приоритет.
	prioNames := map[string]bool{}
	for n := range prio {
		prioNames[n] = true
	}
	if n := strings.ToLower(get("LLM_PROVIDER")); n != "" {
		prioNames[n] = true
	}

	// Любой OpenAI-совместимый сервис одним набором LLM_*: без этого
	// провайдера, которого нет в таблице, подключить было невозможно —
	// именно на этом ломалась Youtoria. Приоритет -1: если человек задал
	// это явно, значит хочет именно его, а не забытый ключ из шаблона.
	if base, key := get("LLM_BASE_URL"), get("LLM_API_KEY"); base != "" || key != "" {
		name := strings.ToLower(firstNonEmpty(get("LLM_PROVIDER"), "custom"))
		kind := "openai"
		switch name {
		case "anthropic", "gemini":
			kind = name
		}
		// Известное имя даёт адрес и модель по умолчанию: иначе
		// LLM_PROVIDER=kilo просил бы у шлюза несуществующий gpt-4o.
		defModel, vision := "gpt-4o", true
		for _, s := range providerSpecs {
			if s.name == name {
				kind = s.kind
				defModel = firstNonEmpty(get(s.envModel), defaultModel(s.name, key, s.defModel))
				vision = visionOK(s.name, key, s.vision)
				if base == "" {
					base = s.defaultBase
				}
			}
		}
		if base == "" {
			return nil, fmt.Errorf("LLM_PROVIDER=%q неизвестен: укажи LLM_BASE_URL", name)
		}
		cfg.Providers = append(cfg.Providers, Provider{
			Name:     name,
			Kind:     kind,
			BaseURL:  strings.TrimRight(base, "/"),
			APIKey:   key,
			Model:    firstNonEmpty(get("LLM_MODEL"), defModel),
			Priority: -1,
			Vision:   vision,
		})
	}

	added := map[string]bool{}
	for _, p := range cfg.Providers {
		added[p.Name] = true
	}

	for _, s := range providerSpecs {
		key := get(s.envKey)
		base := firstNonEmpty(get(s.envBase), s.defaultBase)
		if key == "" && base == "" {
			continue // провайдер не сконфигурирован — просто пропускаем
		}
		// Публичный шлюз без ключа подключаем только по явной просьбе
		// (LLM_PROVIDER/LLM_PRIORITY/KILO_BASE_URL): молча уводить чужие
		// запросы на бесплатный сторонний сервис нельзя.
		if key == "" && !(keyless(s.name) && (get(s.envBase) != "" || prioNames[s.name])) {
			continue
		}
		// "model" из config.json приезжает как LLM_MODEL, поэтому у названного
		// провайдера он тоже должен работать, а не только KILO_MODEL и т. п.
		named := ""
		if strings.EqualFold(get("LLM_PROVIDER"), s.name) {
			named = get("LLM_MODEL")
		}
		p := Provider{
			Name:    s.name,
			Kind:    s.kind,
			BaseURL: strings.TrimRight(base, "/"),
			APIKey:  key,
			Model: firstNonEmpty(
				get(s.envModel), named, defaultModel(s.name, key, s.defModel)),
			Vision: visionOK(s.name, key, s.vision),
		}
		// Не дублируем того, кого уже добавили через LLM_*.
		if added[s.name] {
			continue
		}
		switch v, ok := prio[s.name]; {
		case ok:
			p.Priority = v
		case p.APIKey == "":
			// Локальный сервер без ключа не должен опережать провайдера с ключом:
			// иначе агент уходит на localhost, а в логе — «base_url localhost».
			p.Priority = 200
		default:
			p.Priority = 100 // не указан в LLM_PRIORITY — уходит в конец очереди
		}
		cfg.Providers = append(cfg.Providers, p)
	}
	return cfg, nil
}

func fileExists(path string) bool {
	st, err := os.Stat(path)
	return err == nil && !st.IsDir()
}

// dotenvKey приводит короткие имена из чужих инструкций к нашим:
// `BASE_URL=`/`API_KEY=` раньше молча игнорировались.
func dotenvKey(k string) string {
	switch strings.ToUpper(strings.TrimSpace(k)) {
	case "BASE_URL":
		return "LLM_BASE_URL"
	case "API_KEY":
		return "LLM_API_KEY"
	case "MODEL":
		return "LLM_MODEL"
	case "PROVIDER":
		return "LLM_PROVIDER"
	default:
		return strings.TrimSpace(k)
	}
}

// decodeText снимает BOM и разбирает UTF-16: Блокнот на Windows сохраняет
// так по одному клику, и тогда весь .env выглядит как пустой.
func decodeText(b []byte) []byte {
	switch {
	case bytes.HasPrefix(b, []byte{0xFF, 0xFE}):
		return utf16ToUTF8(b[2:], binary.LittleEndian)
	case bytes.HasPrefix(b, []byte{0xFE, 0xFF}):
		return utf16ToUTF8(b[2:], binary.BigEndian)
	case bytes.HasPrefix(b, []byte{0xEF, 0xBB, 0xBF}):
		return b[3:]
	default:
		return b
	}
}

func utf16ToUTF8(b []byte, order binary.ByteOrder) []byte {
	units := make([]uint16, 0, len(b)/2)
	for i := 0; i+1 < len(b); i += 2 {
		units = append(units, order.Uint16(b[i:i+2]))
	}
	return []byte(string(utf16.Decode(units)))
}

func parseDotenv(path string) (map[string]string, error) {
	out := map[string]string{}
	raw, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			return out, nil // .env не обязателен: ключи могут быть в окружении
		}
		return nil, err
	}

	sc := bufio.NewScanner(bytes.NewReader(decodeText(raw)))
	sc.Buffer(make([]byte, 0, 64*1024), 1024*1024)
	for sc.Scan() {
		line := strings.TrimSpace(sc.Text())
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		line = strings.TrimPrefix(line, "export ")
		i := strings.Index(line, "=")
		if i <= 0 {
			continue
		}
		k := dotenvKey(line[:i])
		v := strings.TrimSpace(line[i+1:])
		if j := strings.Index(v, " #"); j >= 0 {
			v = strings.TrimSpace(v[:j]) // хвостовой комментарий — не часть ключа
		}
		out[k] = strings.Trim(v, `"'`)
	}
	return out, sc.Err()
}

func splitList(s string) []string {
	var out []string
	for _, p := range strings.Split(s, ",") {
		if p = strings.TrimSpace(p); p != "" {
			out = append(out, p)
		}
	}
	return out
}

func firstNonEmpty(vals ...string) string {
	for _, v := range vals {
		if v != "" {
			return v
		}
	}
	return ""
}

func atoiDefault(s string, def int) int {
	if n, err := strconv.Atoi(s); err == nil && n > 0 {
		return n
	}
	return def
}
