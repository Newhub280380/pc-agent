package llm

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"

	"github.com/newhub280380/pc-agent/router/internal/config"
)

// callOpenAICompatible покрывает сразу OpenAI, DeepSeek, Grok(xAI), Qwen,
// OpenRouter, LM Studio, Ollama и llama.cpp — все они говорят на /chat/completions.
//
// Зачем один адаптер на шесть провайдеров: 90% рынка скопировали схему OpenAI.
// Альтернатива — отдельный клиент на каждого — это мёртвый код и лишние баги.
func callOpenAICompatible(ctx context.Context, hc *http.Client, p config.Provider, r Request) (*Response, error) {
	type contentPart struct {
		Type     string            `json:"type"`
		Text     string            `json:"text,omitempty"`
		ImageURL map[string]string `json:"image_url,omitempty"`
	}
	type msg struct {
		Role    string        `json:"role"`
		Content []contentPart `json:"content"`
	}

	body := map[string]any{
		"model":       p.Model,
		"temperature": r.Temperature,
	}
	if r.MaxTokens > 0 {
		body["max_tokens"] = r.MaxTokens
	}
	if r.JSONMode {
		// Не все совместимые сервера поддерживают response_format, поэтому
		// в промпте ядро всё равно требует JSON. Здесь — «мягкое» усиление.
		body["response_format"] = map[string]string{"type": "json_object"}
	}

	msgs := make([]msg, 0, len(r.Messages))
	for _, m := range r.Messages {
		parts := []contentPart{}
		if m.Text != "" {
			parts = append(parts, contentPart{Type: "text", Text: m.Text})
		}
		for _, img := range m.Images {
			mime := m.ImageMIM
			if mime == "" {
				mime = "image/png"
			}
			parts = append(parts, contentPart{
				Type:     "image_url",
				ImageURL: map[string]string{"url": "data:" + mime + ";base64," + img},
			})
		}
		msgs = append(msgs, msg{Role: string(m.Role), Content: parts})
	}
	body["messages"] = msgs

	buf, _ := json.Marshal(body)
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, p.BaseURL+"/chat/completions", bytes.NewReader(buf))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	if p.APIKey != "" {
		req.Header.Set("Authorization", "Bearer "+p.APIKey)
	}
	if p.Name == "openrouter" {
		// OpenRouter требует эти заголовки для корректного роутинга/лимитов.
		req.Header.Set("HTTP-Referer", "https://localhost/pc-agent")
		req.Header.Set("X-Title", "PC Agent")
	}

	resp, err := hc.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	if resp.StatusCode >= 300 {
		return nil, &HTTPError{Code: resp.StatusCode, Body: truncate(string(raw), 800)}
	}

	var parsed struct {
		Choices []struct {
			Message struct {
				Content string `json:"content"`
			} `json:"message"`
		} `json:"choices"`
		Usage struct {
			PromptTokens     int `json:"prompt_tokens"`
			CompletionTokens int `json:"completion_tokens"`
		} `json:"usage"`
	}
	if err := json.Unmarshal(raw, &parsed); err != nil {
		return nil, fmt.Errorf("bad json from %s: %w", p.Name, err)
	}
	if len(parsed.Choices) == 0 {
		return nil, fmt.Errorf("%s: empty choices", p.Name)
	}
	return &Response{
		Text:         parsed.Choices[0].Message.Content,
		Provider:     p.Name,
		Model:        p.Model,
		PromptTokens: parsed.Usage.PromptTokens,
		OutTokens:    parsed.Usage.CompletionTokens,
	}, nil
}

// HTTPError несёт код ответа: по нему роутер решает, ретраить или сразу
// переключаться на другого провайдера (429/5xx — ретрай, 401/400 — фолбэк).
type HTTPError struct {
	Code int
	Body string
}

func (e *HTTPError) Error() string { return fmt.Sprintf("http %d: %s", e.Code, e.Body) }

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}
