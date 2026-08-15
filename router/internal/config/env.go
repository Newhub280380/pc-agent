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
	"os"
	"path/filepath"
	"strconv"
	"strings"
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
	// local: llama.cpp / LM Studio / Ollama в OpenAI-совместимом режиме.
	// Ключ не обязателен, поэтому активируется наличием LOCAL_BASE_URL.
	{"local", "openai", "LOCAL_API_KEY", "LOCAL_MODEL", "LOCAL_BASE_URL", "", "qwen2.5-vl-7b", true},
}

// Load читает .env (если есть) и переменные процесса.
// Переменные процесса имеют приоритет над файлом — так удобнее отлаживать.
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

	cfg := &Config{
		Listen:     firstNonEmpty(get("ROUTER_LISTEN"), "127.0.0.1:8713"),
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

	for _, s := range providerSpecs {
		key := get(s.envKey)
		base := firstNonEmpty(get(s.envBase), s.defaultBase)
		if key == "" && base == "" {
			continue // провайдер не сконфигурирован — просто пропускаем
		}
		if key == "" && s.name != "local" {
			continue
		}
		p := Provider{
			Name:    s.name,
			Kind:    s.kind,
			BaseURL: strings.TrimRight(base, "/"),
			APIKey:  key,
			Model:   firstNonEmpty(get(s.envModel), s.defModel),
			Vision:  s.vision,
		}
		if v, ok := prio[s.name]; ok {
			p.Priority = v
		} else {
			p.Priority = 100 // не указан в LLM_PRIORITY — уходит в конец очереди
		}
		cfg.Providers = append(cfg.Providers, p)
	}
	return cfg, nil
}

func parseDotenv(path string) (map[string]string, error) {
	out := map[string]string{}
	f, err := os.Open(path)
	if err != nil {
		if os.IsNotExist(err) {
			return out, nil // .env не обязателен: ключи могут быть в окружении
		}
		return nil, err
	}
	defer f.Close()

	sc := bufio.NewScanner(f)
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
		k := strings.TrimSpace(line[:i])
		v := strings.TrimSpace(line[i+1:])
		v = strings.Trim(v, `"'`)
		out[k] = v
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
