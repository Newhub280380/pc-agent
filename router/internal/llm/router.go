package llm

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"math/rand"
	"net"
	"net/http"
	"net/url"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/newhub280380/pc-agent/router/internal/config"
)

// Router выбирает провайдера, ретраит и делает фолбэк на следующего.
//
// Зачем circuit breaker: если у Anthropic упал регион, тупой ретрай по кругу
// съест минуты на каждом шаге агента. Провайдер, отдавший 5xx/429 подряд,
// «выпадает» на cooldown, и агент продолжает работать на другом.
//
// Альтернативы, которые рассматривались:
//  1. Просто первый рабочий ключ — дёшево, но нет отказоустойчивости.
//  2. Гонка запросов ко всем провайдерам (hedged requests) — минимальная
//     задержка, но платим за все ответы. Оставлено на v2 как опция для
//     Purpose="act", где задержка критична.
type Router struct {
	cfg  *config.Config
	http *http.Client
	log  *slog.Logger

	mu    sync.Mutex
	state map[string]*breaker
}

type breaker struct {
	failures  int
	openUntil time.Time
}

func New(cfg *config.Config, log *slog.Logger) *Router {
	return &Router{
		cfg: cfg,
		log: log,
		http: &http.Client{
			Timeout: time.Duration(cfg.TimeoutSec) * time.Second,
			// Зачем свой транспорт: держим keep-alive соединения к API,
			// иначе на каждом шаге агента платим за TLS-handshake (~200мс).
			Transport: &http.Transport{
				MaxIdleConns:        32,
				MaxIdleConnsPerHost: 8,
				IdleConnTimeout:     90 * time.Second,
			},
		},
		state: map[string]*breaker{},
	}
}

// Providers возвращает список активных провайдеров (для GUI и диагностики).
func (r *Router) Providers() []config.Provider { return r.cfg.Providers }

func (r *Router) candidates(req Request) []config.Provider {
	var out []config.Provider
	for _, p := range r.cfg.Providers {
		if req.ForceProvide != "" && p.Name != req.ForceProvide {
			continue
		}
		if req.NeedVision && !p.Vision {
			continue // бессмысленно слать скриншот в текстовую модель
		}
		if r.isOpen(p.Name) {
			continue
		}
		out = append(out, p)
	}
	sort.SliceStable(out, func(i, j int) bool { return out[i].Priority < out[j].Priority })
	return out
}

func (r *Router) isOpen(name string) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	b, ok := r.state[name]
	return ok && time.Now().Before(b.openUntil)
}

func (r *Router) penalize(name string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	b, ok := r.state[name]
	if !ok {
		b = &breaker{}
		r.state[name] = b
	}
	b.failures++
	if b.failures >= 3 {
		// Экспоненциальный cooldown, но не дольше 5 минут: агент не должен
		// навсегда терять провайдера из-за одного плохого окна.
		d := time.Duration(1<<min(b.failures-3, 5)) * 10 * time.Second
		if d > 5*time.Minute {
			d = 5 * time.Minute
		}
		b.openUntil = time.Now().Add(d)
	}
}

func (r *Router) reward(name string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	delete(r.state, name)
}

// Complete — единственная точка входа для ядра.
func (r *Router) Complete(ctx context.Context, req Request) (*Response, error) {
	cands := r.candidates(req)
	if len(cands) == 0 {
		// Явное сообщение вместо тихого отказа: агенту нужно понять,
		// что просить у пользователя (ключ/модель с vision).
		if req.NeedVision {
			return nil, errors.New("нет доступного vision-провайдера: добавь ключ (OPENAI/ANTHROPIC/GEMINI/QWEN/OPENROUTER) в .env")
		}
		return nil, errors.New("нет доступных LLM-провайдеров: заполни .env")
	}

	attempts := 0
	var lastErr error
	unreachable := []string{}
	for _, p := range cands {
		// Пинг до ретраев: три таймаута по 120с на мёртвом адресе выглядят как
		// «не вышло за 3 попытки», хотя причина — недоступный сервер или опечатка
		// в base_url. Проверяем только TCP+TLS, ключ никуда не уходит.
		if err := reachable(ctx, p.BaseURL); err != nil {
			unreachable = append(unreachable, p.Name+" "+p.BaseURL)
			lastErr = err
			r.log.Warn("llm unreachable", "provider", p.Name, "base_url", p.BaseURL, "err", err.Error())
			continue
		}
		for try := 0; try < r.cfg.MaxRetries; try++ {
			attempts++
			start := time.Now()
			resp, err := r.dispatch(ctx, p, req)
			if err == nil {
				r.reward(p.Name)
				resp.LatencyMS = time.Since(start).Milliseconds()
				resp.Attempts = attempts
				r.log.Info("llm ok",
					"provider", p.Name, "model", p.Model, "purpose", req.Purpose,
					"ms", resp.LatencyMS, "in", resp.PromptTokens, "out", resp.OutTokens)
				return resp, nil
			}
			lastErr = err
			r.log.Warn("llm fail", "provider", p.Name, "purpose", req.Purpose, "try", try+1, "err", err.Error())

			if !retryable(err) {
				break // 400/401/404: ретрай не поможет, идём к следующему провайдеру
			}
			r.penalize(p.Name)
			// Джиттер, чтобы не долбить API синхронно после общего сбоя.
			sleep := time.Duration(300*(1<<try))*time.Millisecond + time.Duration(rand.Intn(250))*time.Millisecond
			select {
			case <-ctx.Done():
				return nil, ctx.Err()
			case <-time.After(sleep):
			}
		}
		r.penalize(p.Name)
	}
	if attempts == 0 && len(unreachable) > 0 {
		return nil, fmt.Errorf("Сервер LLM недоступен (%s): %w", strings.Join(unreachable, ", "), lastErr)
	}
	return nil, fmt.Errorf("все провайдеры отказали (%d попыток): %w", attempts, lastErr)
}

// reachable проверяет только сеть: любой ответ сервера (даже 401/404)
// считается доступностью — такие ошибки должен объяснять реальный запрос,
// а не пинг.
func reachable(ctx context.Context, baseURL string) error {
	u, err := url.Parse(baseURL)
	if err != nil || u.Host == "" || u.Scheme == "" {
		// Чаще всего это лишние символы из .env: кавычки, пробел, BOM или
		// адрес без схемы. Печатаем значение как есть — так видно мусор.
		return fmt.Errorf("неверный base_url %q: нужен вид https://host/v1", baseURL)
	}
	port := u.Port()
	if port == "" {
		port = "443"
		if strings.EqualFold(u.Scheme, "http") {
			port = "80"
		}
	}
	d := net.Dialer{Timeout: 5 * time.Second}
	conn, err := d.DialContext(ctx, "tcp", net.JoinHostPort(u.Hostname(), port))
	if err != nil {
		return err
	}
	return conn.Close()
}

func (r *Router) dispatch(ctx context.Context, p config.Provider, req Request) (*Response, error) {
	switch p.Kind {
	case "anthropic":
		return callAnthropic(ctx, r.http, p, req)
	case "gemini":
		return callGemini(ctx, r.http, p, req)
	default:
		return callOpenAICompatible(ctx, r.http, p, req)
	}
}

func retryable(err error) bool {
	var he *HTTPError
	if errors.As(err, &he) {
		return he.Code == 408 || he.Code == 409 || he.Code == 429 || he.Code >= 500
	}
	return true // сетевые/таймаут — ретраим
}

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}
