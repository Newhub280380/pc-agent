package config

import (
	"os"
	"path/filepath"
	"testing"
)

func writeEnv(t *testing.T, body string) string {
	t.Helper()
	p := filepath.Join(t.TempDir(), ".env")
	if err := os.WriteFile(p, []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
	return p
}

func TestLoadPicksOnlyConfiguredProvidersInPriorityOrder(t *testing.T) {
	p := writeEnv(t, `
# комментарий
OPENAI_API_KEY="sk-test"
ANTHROPIC_API_KEY=ak-test
GEMINI_API_KEY=
LLM_PRIORITY=anthropic,openai
ROUTER_TOKEN=tok
`)
	cfg, err := Load(p)
	if err != nil {
		t.Fatal(err)
	}
	if len(cfg.Providers) != 2 {
		t.Fatalf("ожидалось 2 провайдера, получено %d", len(cfg.Providers))
	}
	byName := map[string]Provider{}
	for _, pr := range cfg.Providers {
		byName[pr.Name] = pr
	}
	if byName["anthropic"].Priority >= byName["openai"].Priority {
		t.Fatal("LLM_PRIORITY не применился")
	}
	if byName["openai"].APIKey != "sk-test" {
		t.Fatalf("кавычки не сняты: %q", byName["openai"].APIKey)
	}
	if cfg.AuthToken != "tok" || cfg.Listen != "127.0.0.1:8713" {
		t.Fatalf("дефолты сломаны: %+v", cfg)
	}
}

func TestLocalProviderNeedsOnlyBaseURL(t *testing.T) {
	// LM Studio/Ollama обычно без ключа — провайдер должен подняться всё равно.
	cfg, err := Load(writeEnv(t, "LOCAL_BASE_URL=http://127.0.0.1:1234/v1\n"))
	if err != nil {
		t.Fatal(err)
	}
	if len(cfg.Providers) != 1 || cfg.Providers[0].Name != "local" {
		t.Fatalf("local не поднялся: %+v", cfg.Providers)
	}
}

func TestUTF16EnvFromNotepadIsParsed(t *testing.T) {
	// Блокнот сохраняет "Unicode" как UTF-16LE с BOM: раньше весь .env
	// выглядел пустым и агент шёл без ключа.
	body := []byte{0xFF, 0xFE}
	for _, r := range "OPENAI_API_KEY=sk-utf16\n" {
		body = append(body, byte(r), 0x00)
	}
	p := filepath.Join(t.TempDir(), ".env")
	if err := os.WriteFile(p, body, 0o600); err != nil {
		t.Fatal(err)
	}
	cfg, err := Load(p)
	if err != nil {
		t.Fatal(err)
	}
	if len(cfg.Providers) != 1 || cfg.Providers[0].APIKey != "sk-utf16" {
		t.Fatalf("UTF-16 .env не прочитан: %+v", cfg.Providers)
	}
}

func TestShortNamesAndTrailingComment(t *testing.T) {
	cfg, err := Load(writeEnv(t, "\ufeffBASE_URL=https://api.openai.com/v1 # комментарий\nAPI_KEY=sk-short\n"))
	if err != nil {
		t.Fatal(err)
	}
	if len(cfg.Providers) == 0 {
		t.Fatal("короткие имена BASE_URL/API_KEY проигнорированы")
	}
	p := cfg.Providers[0]
	if p.BaseURL != "https://api.openai.com/v1" || p.APIKey != "sk-short" {
		t.Fatalf("значения разобраны неверно: %+v", p)
	}
}

func TestKeylessLocalServerRanksBehindKeyedProvider(t *testing.T) {
	// Без LLM_PRIORITY локальный сервер не должен перебивать ключ OpenAI —
	// иначе запросы уходят на localhost.
	cfg, err := Load(writeEnv(t, "OPENAI_API_KEY=sk-test\nLOCAL_BASE_URL=http://localhost:20128/v1\n"))
	if err != nil {
		t.Fatal(err)
	}
	byName := map[string]Provider{}
	for _, pr := range cfg.Providers {
		byName[pr.Name] = pr
	}
	if byName["openai"].Priority >= byName["local"].Priority {
		t.Fatalf("local не должен опережать openai: %+v", cfg.Providers)
	}
}

func TestMissingEnvFileIsNotAnError(t *testing.T) {
	// Ключи могут быть заданы переменными процесса — отсутствие файла законно.
	cfg, err := Load(filepath.Join(t.TempDir(), "нет.env"))
	if err != nil {
		t.Fatal(err)
	}
	if len(cfg.Providers) != 0 {
		t.Fatalf("ожидался пустой список: %+v", cfg.Providers)
	}
}

func TestYoutoriaProviderFromKeyAlone(t *testing.T) {
	cfg, err := Load(writeEnv(t, "YOUTORIA_API_KEY=sk-y\n"))
	if err != nil {
		t.Fatal(err)
	}
	if len(cfg.Providers) != 1 {
		t.Fatalf("ожидался 1 провайдер: %+v", cfg.Providers)
	}
	p := cfg.Providers[0]
	if p.Name != "youtoria" || p.Kind != "openai" || p.BaseURL != "https://api.youtoria.ai/v1" {
		t.Fatalf("youtoria настроена неверно: %+v", p)
	}
}

func TestNvidiaNimProviderFromKeyAlone(t *testing.T) {
	cfg, err := Load(writeEnv(t, "NVIDIA_API_KEY=nvapi-x\n"))
	if err != nil {
		t.Fatal(err)
	}
	if len(cfg.Providers) != 1 {
		t.Fatalf("ожидался 1 провайдер: %+v", cfg.Providers)
	}
	p := cfg.Providers[0]
	if p.Name != "nvidia" || p.Kind != "openai" ||
		p.BaseURL != "https://integrate.api.nvidia.com/v1" {
		t.Fatalf("nvidia настроена неверно: %+v", p)
	}
	if p.Model != "meta/llama-3.2-90b-vision-instruct" || !p.Vision {
		t.Fatalf("нужна vision-модель по умолчанию: %+v", p)
	}

	cfg, err = Load(writeEnv(t, "NVIDIA_API_KEY=nvapi-x\nNVIDIA_MODEL=nvidia/nemotron-nano-12b-v2-vl\n"))
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Providers[0].Model != "nvidia/nemotron-nano-12b-v2-vl" {
		t.Fatalf("NVIDIA_MODEL проигнорирован: %+v", cfg.Providers[0])
	}
}

func TestKiloGatewayWithoutKeyNeedsExplicitRequest(t *testing.T) {
	// Без просьбы бесплатный шлюз не подключается: чужие запросы не должны
	// молча уходить на сторонний сервис.
	cfg, err := Load(writeEnv(t, "OPENAI_API_KEY=sk-o\n"))
	if err != nil {
		t.Fatal(err)
	}
	for _, p := range cfg.Providers {
		if p.Name == "kilo" {
			t.Fatalf("kilo подключился без просьбы: %+v", cfg.Providers)
		}
	}

	cfg, err = Load(writeEnv(t, "LLM_PROVIDER=kilo\n"))
	if err != nil {
		t.Fatal(err)
	}
	if len(cfg.Providers) != 1 {
		t.Fatalf("ожидался только kilo: %+v", cfg.Providers)
	}
	p := cfg.Providers[0]
	if p.BaseURL != "https://api.kilo.ai/api/gateway" || p.APIKey != "" {
		t.Fatalf("kilo настроен неверно: %+v", p)
	}
	if p.Model != "kilo-auto/free" {
		t.Fatalf("без ключа нужна бесплатная модель, получено %q", p.Model)
	}
	// Бесплатный уровень отвергает картинки, поэтому шаги со скриншотом
	// должны обходить его стороной, а не падать на 400.
	if p.Vision {
		t.Fatalf("бесплатный уровень не умеет vision: %+v", p)
	}

	cfg, err = Load(writeEnv(t, "KILO_API_KEY=sk-k\n"))
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Providers[0].Model != "kilo-auto/frontier" || !cfg.Providers[0].Vision {
		t.Fatalf("с ключом ожидался frontier с vision: %+v", cfg.Providers[0])
	}

	// Явная модель важнее подстановок в обе стороны.
	cfg, err = Load(writeEnv(t, "LLM_PROVIDER=kilo\nKILO_MODEL=stepfun/step-3.7-flash:free\n"))
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Providers[0].Model != "stepfun/step-3.7-flash:free" {
		t.Fatalf("KILO_MODEL проигнорирован: %+v", cfg.Providers[0])
	}
}

func TestKiloThroughGenericLLMBlockKeepsGatewayModel(t *testing.T) {
	// Тот же шлюз, заданный общими LLM_*: подставлять gpt-4o нельзя —
	// у шлюза такой модели нет, запрос вернул бы ошибку модели.
	cfg, err := Load(writeEnv(t, "LLM_PROVIDER=kilo\nLLM_BASE_URL=https://api.kilo.ai/api/gateway\n"))
	if err != nil {
		t.Fatal(err)
	}
	p := cfg.Providers[0]
	if p.Name != "kilo" || p.Model != "kilo-auto/free" || p.Vision {
		t.Fatalf("kilo через LLM_* настроен неверно: %+v", p)
	}
}

func TestGenericLLMBlockConfiguresAnyProvider(t *testing.T) {
	// Главный сценарий бага: провайдера нет в таблице, задан только LLM_*.
	cfg, err := Load(writeEnv(t, `
LLM_PROVIDER=youtoria
LLM_BASE_URL=https://api.youtoria.ai/v1
LLM_API_KEY=sk-json
LLM_MODEL=gpt-4o
OPENAI_API_KEY=sk-old
`))
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Providers[0].Name != "youtoria" || cfg.Providers[0].Priority != -1 {
		t.Fatalf("LLM_* не стал первым: %+v", cfg.Providers)
	}
	if cfg.Providers[0].Model != "gpt-4o" || cfg.Providers[0].APIKey != "sk-json" {
		t.Fatalf("значения LLM_* потеряны: %+v", cfg.Providers[0])
	}
	// Дубля youtoria быть не должно, а openai остаётся резервом.
	seen := map[string]int{}
	for _, p := range cfg.Providers {
		seen[p.Name]++
	}
	if seen["youtoria"] != 1 || seen["openai"] != 1 {
		t.Fatalf("дубли/потери провайдеров: %+v", seen)
	}
}

func TestUnknownProviderWithoutBaseURLIsRejected(t *testing.T) {
	_, err := Load(writeEnv(t, "LLM_PROVIDER=неведомый\nLLM_API_KEY=sk-x\n"))
	if err == nil {
		t.Fatal("нужна понятная ошибка вместо тихого падения на запросе")
	}
}

func TestProcessEnvBeatsDotenv(t *testing.T) {
	t.Setenv("OPENAI_API_KEY", "sk-from-env")
	cfg, err := Load(writeEnv(t, "OPENAI_API_KEY=sk-from-file\n"))
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Providers[0].APIKey != "sk-from-env" {
		t.Fatalf("приоритет окружения сломан: %+v", cfg.Providers[0])
	}
}
