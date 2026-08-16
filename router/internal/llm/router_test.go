package llm

import (
	"context"
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/newhub280380/pc-agent/router/internal/config"
)

func quietLogger() *slog.Logger {
	return slog.New(slog.NewTextHandler(io.Discard, nil))
}

func openaiStub(t *testing.T, status int, body string, hits *int) *httptest.Server {
	t.Helper()
	s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		*hits++
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(status)
		_, _ = io.WriteString(w, body)
	}))
	t.Cleanup(s.Close)
	return s
}

const okBody = `{"choices":[{"message":{"content":"привет"}}],"usage":{"prompt_tokens":5,"completion_tokens":2}}`

func TestFallbackToNextProviderOnServerError(t *testing.T) {
	var badHits, goodHits int
	bad := openaiStub(t, 500, `{"error":{"message":"down"}}`, &badHits)
	good := openaiStub(t, 200, okBody, &goodHits)

	r := New(&config.Config{
		MaxRetries: 2,
		TimeoutSec: 5,
		Providers: []config.Provider{
			{Name: "bad", Kind: "openai", BaseURL: bad.URL, APIKey: "k", Model: "m", Priority: 0, Vision: true},
			{Name: "good", Kind: "openai", BaseURL: good.URL, APIKey: "k", Model: "m", Priority: 1, Vision: true},
		},
	}, quietLogger())

	resp, err := r.Complete(context.Background(), Request{Messages: []Message{{Role: RoleUser, Text: "hi"}}})
	if err != nil {
		t.Fatalf("фолбэк не сработал: %v", err)
	}
	if resp.Provider != "good" || resp.Text != "привет" {
		t.Fatalf("ответ не от резервного провайдера: %+v", resp)
	}
	if badHits != 2 {
		t.Fatalf("5xx должен ретраиться MaxRetries раз, было %d", badHits)
	}
}

func TestAuthErrorIsNotRetried(t *testing.T) {
	var hits int
	bad := openaiStub(t, 401, `{"error":{"message":"bad key"}}`, &hits)

	r := New(&config.Config{
		MaxRetries: 3,
		TimeoutSec: 5,
		Providers:  []config.Provider{{Name: "bad", Kind: "openai", BaseURL: bad.URL, APIKey: "k", Model: "m"}},
	}, quietLogger())

	if _, err := r.Complete(context.Background(), Request{Messages: []Message{{Role: RoleUser, Text: "hi"}}}); err == nil {
		t.Fatal("ожидалась ошибка")
	}
	if hits != 1 {
		t.Fatalf("401 ретраить бессмысленно, попыток: %d", hits)
	}
}

func TestVisionRequestSkipsTextOnlyProviders(t *testing.T) {
	r := New(&config.Config{
		MaxRetries: 1,
		TimeoutSec: 5,
		Providers:  []config.Provider{{Name: "text", Kind: "openai", BaseURL: "http://127.0.0.1:1", APIKey: "k", Model: "m", Vision: false}},
	}, quietLogger())

	_, err := r.Complete(context.Background(), Request{NeedVision: true, Messages: []Message{{Role: RoleUser, Text: "hi"}}})
	if err == nil {
		t.Fatal("ожидалась явная ошибка про отсутствие vision-провайдера")
	}
}

func TestBreakerOpensAfterRepeatedFailures(t *testing.T) {
	var hits int
	bad := openaiStub(t, 503, `{"error":{"message":"down"}}`, &hits)

	r := New(&config.Config{
		MaxRetries: 3,
		TimeoutSec: 5,
		Providers:  []config.Provider{{Name: "bad", Kind: "openai", BaseURL: bad.URL, APIKey: "k", Model: "m"}},
	}, quietLogger())

	req := Request{Messages: []Message{{Role: RoleUser, Text: "hi"}}}
	_, _ = r.Complete(context.Background(), req)
	before := hits
	// Провайдер на cooldown — второй вызов не должен трогать сеть вообще.
	_, err := r.Complete(context.Background(), req)
	if err == nil {
		t.Fatal("ожидалась ошибка при разомкнутой цепи")
	}
	if hits != before {
		t.Fatalf("circuit breaker не разомкнулся: %d -> %d", before, hits)
	}
}

func TestUnreachableServerReportsDownInsteadOfRetries(t *testing.T) {
	// Порт закрыт: раньше это выглядело как «не вышло за 3 попытки».
	var hits int
	dead := openaiStub(t, 200, okBody, &hits)
	url := dead.URL
	dead.Close()

	r := New(&config.Config{
		MaxRetries: 3,
		TimeoutSec: 5,
		Providers:  []config.Provider{{Name: "youtoria", Kind: "openai", BaseURL: url, APIKey: "k", Model: "m"}},
	}, quietLogger())

	_, err := r.Complete(context.Background(), Request{Purpose: "act", Messages: []Message{{Role: "user", Text: "привет"}}})
	if err == nil {
		t.Fatal("ожидалась ошибка")
	}
	if !strings.Contains(err.Error(), "Сервер LLM недоступен") {
		t.Fatalf("неинформативная ошибка: %v", err)
	}
	if hits != 0 {
		t.Fatalf("до мёртвого сервера не должно быть запросов, было %d", hits)
	}
}

func TestReachableServerStillReportsAuthError(t *testing.T) {
	// 401 — доказательство, что сервер жив: нельзя писать «недоступен».
	var hits int
	s := openaiStub(t, 401, `{"error":{"message":"bad key"}}`, &hits)
	r := New(&config.Config{
		MaxRetries: 3,
		TimeoutSec: 5,
		Providers:  []config.Provider{{Name: "youtoria", Kind: "openai", BaseURL: s.URL, APIKey: "k", Model: "m"}},
	}, quietLogger())

	_, err := r.Complete(context.Background(), Request{Purpose: "act", Messages: []Message{{Role: "user", Text: "привет"}}})
	if err == nil || strings.Contains(err.Error(), "недоступен") {
		t.Fatalf("401 не должен превращаться в «недоступен»: %v", err)
	}
	if hits == 0 {
		t.Fatal("запрос до живого сервера не дошёл")
	}
}
