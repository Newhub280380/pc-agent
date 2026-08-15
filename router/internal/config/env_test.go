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
