// Package server: локальный HTTP-фасад роутера для Rust-ядра.
//
// Зачем HTTP, а не FFI/gRPC:
//   - Go-код собирается в отдельный .exe без CGO (условие: 1 файл, статика);
//   - падение сетевого слоя не роняет ядро агента (изоляция процессов);
//   - можно перезапустить/обновить роутер, не убивая текущую задачу.
//
// Слушаем строго 127.0.0.1 + требуем локальный токен — чтобы никакая
// страница в браузере не смогла дёргать наш роутер с ключами.
package server

import (
	"context"
	"crypto/subtle"
	"encoding/json"
	"log/slog"
	"net/http"
	"time"

	"github.com/newhub280380/pc-agent/router/internal/config"
	"github.com/newhub280380/pc-agent/router/internal/llm"
)

type Server struct {
	cfg    *config.Config
	router *llm.Router
	log    *slog.Logger
}

func New(cfg *config.Config, r *llm.Router, log *slog.Logger) *Server {
	return &Server{cfg: cfg, router: r, log: log}
}

func (s *Server) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/health", s.handleHealth)
	mux.HandleFunc("/v1/providers", s.auth(s.handleProviders))
	mux.HandleFunc("/v1/complete", s.auth(s.handleComplete))
	return mux
}

// Максимальный размер запроса от ядра. Реальный промпт со скриншотом в base64
// — единицы мегабайт; 32 МиБ с запасом. Без лимита любой локальный процесс
// (или вкладка браузера через fetch) кладёт роутер одним POST-ом.
const maxBodyBytes = 32 << 20

func (s *Server) auth(next http.HandlerFunc) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		// Fail-closed: пустой токен в конфиге — не «режим без пароля», а
		// сломанная установка. Иначе любой локальный процесс получает доступ
		// к чужим API-ключам.
		if s.cfg.AuthToken == "" {
			http.Error(w, "router misconfigured: ROUTER_TOKEN is empty", http.StatusForbidden)
			return
		}
		got := r.Header.Get("X-Agent-Token")
		if subtle.ConstantTimeCompare([]byte(got), []byte(s.cfg.AuthToken)) != 1 {
			http.Error(w, "forbidden", http.StatusForbidden)
			return
		}
		next(w, r)
	}
}

func (s *Server) handleHealth(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{
		"status":    "ok",
		"providers": len(s.router.Providers()),
	})
}

func (s *Server) handleProviders(w http.ResponseWriter, _ *http.Request) {
	type item struct {
		Name     string `json:"name"`
		Model    string `json:"model"`
		Vision   bool   `json:"vision"`
		Priority int    `json:"priority"`
	}
	out := []item{}
	for _, p := range s.router.Providers() {
		out = append(out, item{p.Name, p.Model, p.Vision, p.Priority})
	}
	writeJSON(w, http.StatusOK, out)
}

func (s *Server) handleComplete(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	r.Body = http.MaxBytesReader(w, r.Body, maxBodyBytes)
	var req llm.Request
	if err := json.NewDecoder(r.Body).Decode(&req); err != nil {
		writeJSON(w, http.StatusBadRequest, llm.Response{Error: "bad request: " + err.Error()})
		return
	}
	ctx, cancel := context.WithTimeout(r.Context(), time.Duration(s.cfg.TimeoutSec+30)*time.Second)
	defer cancel()

	resp, err := s.router.Complete(ctx, req)
	if err != nil {
		// 502, а не 500: ошибка вне нашего процесса. Ядро по этому коду
		// понимает, что задачу можно повторить позже, а не «код сломан».
		writeJSON(w, http.StatusBadGateway, llm.Response{Error: err.Error()})
		return
	}
	writeJSON(w, http.StatusOK, resp)
}

func writeJSON(w http.ResponseWriter, code int, v any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(code)
	_ = json.NewEncoder(w).Encode(v)
}
