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

// callAnthropic — адаптер Claude Messages API.
// Отличия от OpenAI, из-за которых нужен отдельный код:
//   - system-промпт вынесен в отдельное поле, а не в messages;
//   - картинки идут как {"type":"image","source":{...base64}};
//   - авторизация через x-api-key + anthropic-version.
func callAnthropic(ctx context.Context, hc *http.Client, p config.Provider, r Request) (*Response, error) {
	type source struct {
		Type      string `json:"type"`
		MediaType string `json:"media_type"`
		Data      string `json:"data"`
	}
	type part struct {
		Type   string  `json:"type"`
		Text   string  `json:"text,omitempty"`
		Source *source `json:"source,omitempty"`
	}
	type msg struct {
		Role    string `json:"role"`
		Content []part `json:"content"`
	}

	var system string
	msgs := make([]msg, 0, len(r.Messages))
	for _, m := range r.Messages {
		if m.Role == RoleSystem {
			if system != "" {
				system += "\n\n"
			}
			system += m.Text
			continue
		}
		parts := []part{}
		for _, img := range m.Images {
			mime := m.ImageMIM
			if mime == "" {
				mime = "image/png"
			}
			parts = append(parts, part{Type: "image", Source: &source{Type: "base64", MediaType: mime, Data: img}})
		}
		if m.Text != "" {
			parts = append(parts, part{Type: "text", Text: m.Text})
		}
		msgs = append(msgs, msg{Role: string(m.Role), Content: parts})
	}

	maxTok := r.MaxTokens
	if maxTok <= 0 {
		maxTok = 4096 // Anthropic требует max_tokens обязательным полем
	}
	body := map[string]any{
		"model":       p.Model,
		"max_tokens":  maxTok,
		"temperature": r.Temperature,
		"messages":    msgs,
	}
	if system != "" {
		body["system"] = system
	}

	buf, _ := json.Marshal(body)
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, p.BaseURL+"/messages", bytes.NewReader(buf))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("x-api-key", p.APIKey)
	req.Header.Set("anthropic-version", "2023-06-01")

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
		Content []struct {
			Type string `json:"type"`
			Text string `json:"text"`
		} `json:"content"`
		Usage struct {
			InputTokens  int `json:"input_tokens"`
			OutputTokens int `json:"output_tokens"`
		} `json:"usage"`
	}
	if err := json.Unmarshal(raw, &parsed); err != nil {
		return nil, fmt.Errorf("bad json from anthropic: %w", err)
	}
	out := ""
	for _, c := range parsed.Content {
		if c.Type == "text" {
			out += c.Text
		}
	}
	if out == "" {
		return nil, fmt.Errorf("anthropic: empty content")
	}
	return &Response{
		Text:         out,
		Provider:     p.Name,
		Model:        p.Model,
		PromptTokens: parsed.Usage.InputTokens,
		OutTokens:    parsed.Usage.OutputTokens,
	}, nil
}
