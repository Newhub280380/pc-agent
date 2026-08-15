package server

import (
	"io"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/newhub280380/pc-agent/router/internal/config"
	"github.com/newhub280380/pc-agent/router/internal/llm"
)

func newTestServer(token string) http.Handler {
	cfg := &config.Config{AuthToken: token, TimeoutSec: 1}
	log := slog.New(slog.NewTextHandler(io.Discard, nil))
	return New(cfg, llm.New(cfg, log), log).Handler()
}

// Пустой токен раньше означал «пускаем всех» — то есть любой локальный
// процесс мог тратить чужие ключи. Теперь это ошибка конфигурации.
func TestEmptyTokenFailsClosed(t *testing.T) {
	h := newTestServer("")
	for _, path := range []string{"/v1/providers", "/v1/complete"} {
		w := httptest.NewRecorder()
		h.ServeHTTP(w, httptest.NewRequest(http.MethodPost, path, strings.NewReader("{}")))
		if w.Code != http.StatusForbidden {
			t.Errorf("%s: код %d, ждали 403", path, w.Code)
		}
	}
}

func TestWrongTokenRejected(t *testing.T) {
	h := newTestServer("secret")
	w := httptest.NewRecorder()
	r := httptest.NewRequest(http.MethodGet, "/v1/providers", nil)
	r.Header.Set("X-Agent-Token", "nope")
	h.ServeHTTP(w, r)
	if w.Code != http.StatusForbidden {
		t.Fatalf("код %d, ждали 403", w.Code)
	}

	w = httptest.NewRecorder()
	r = httptest.NewRequest(http.MethodGet, "/v1/providers", nil)
	r.Header.Set("X-Agent-Token", "secret")
	h.ServeHTTP(w, r)
	if w.Code != http.StatusOK {
		t.Fatalf("код %d, ждали 200", w.Code)
	}
}

// /health намеренно без токена: по нему ядро проверяет, что роутер жив.
func TestHealthIsOpen(t *testing.T) {
	w := httptest.NewRecorder()
	newTestServer("").ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/health", nil))
	if w.Code != http.StatusOK {
		t.Fatalf("код %d, ждали 200", w.Code)
	}
}

// Гигантское тело не должно съедать память процесса.
func TestOversizedBodyRejected(t *testing.T) {
	h := newTestServer("secret")
	body := `{"prompt":"` + strings.Repeat("a", maxBodyBytes+1024) + `"}`
	w := httptest.NewRecorder()
	r := httptest.NewRequest(http.MethodPost, "/v1/complete", strings.NewReader(body))
	r.Header.Set("X-Agent-Token", "secret")
	h.ServeHTTP(w, r)
	if w.Code != http.StatusBadRequest && w.Code != http.StatusRequestEntityTooLarge {
		t.Fatalf("код %d, ждали 400/413", w.Code)
	}
}

// Кривой JSON от ядра — это 400, а не паника.
func TestGarbageBodies(t *testing.T) {
	h := newTestServer("secret")
	for _, b := range []string{"", "{", "]", "null", `{"messages":`, "\x00\x01"} {
		w := httptest.NewRecorder()
		r := httptest.NewRequest(http.MethodPost, "/v1/complete", strings.NewReader(b))
		r.Header.Set("X-Agent-Token", "secret")
		h.ServeHTTP(w, r)
		if w.Code == http.StatusOK {
			t.Errorf("тело %q прошло как валидное", b)
		}
	}
}
