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

// callGemini — адаптер Google Generative Language API.
// Отличия: ключ в query-параметре, роль ассистента называется "model",
// контент лежит в parts[] с inline_data для картинок.
func callGemini(ctx context.Context, hc *http.Client, p config.Provider, r Request) (*Response, error) {
	type inlineData struct {
		MimeType string `json:"mime_type"`
		Data     string `json:"data"`
	}
	type part struct {
		Text       string      `json:"text,omitempty"`
		InlineData *inlineData `json:"inline_data,omitempty"`
	}
	type content struct {
		Role  string `json:"role"`
		Parts []part `json:"parts"`
	}

	var sysParts []part
	var contents []content
	for _, m := range r.Messages {
		if m.Role == RoleSystem {
			sysParts = append(sysParts, part{Text: m.Text})
			continue
		}
		role := "user"
		if m.Role == RoleAssistant {
			role = "model"
		}
		var parts []part
		if m.Text != "" {
			parts = append(parts, part{Text: m.Text})
		}
		for _, img := range m.Images {
			mime := m.ImageMIM
			if mime == "" {
				mime = "image/png"
			}
			parts = append(parts, part{InlineData: &inlineData{MimeType: mime, Data: img}})
		}
		contents = append(contents, content{Role: role, Parts: parts})
	}

	genCfg := map[string]any{"temperature": r.Temperature}
	if r.MaxTokens > 0 {
		genCfg["maxOutputTokens"] = r.MaxTokens
	}
	if r.JSONMode {
		genCfg["responseMimeType"] = "application/json"
	}
	body := map[string]any{"contents": contents, "generationConfig": genCfg}
	if len(sysParts) > 0 {
		body["systemInstruction"] = map[string]any{"parts": sysParts}
	}

	buf, _ := json.Marshal(body)
	url := fmt.Sprintf("%s/models/%s:generateContent?key=%s", p.BaseURL, p.Model, p.APIKey)
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(buf))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")

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
		Candidates []struct {
			Content struct {
				Parts []struct {
					Text string `json:"text"`
				} `json:"parts"`
			} `json:"content"`
		} `json:"candidates"`
		UsageMetadata struct {
			PromptTokenCount     int `json:"promptTokenCount"`
			CandidatesTokenCount int `json:"candidatesTokenCount"`
		} `json:"usageMetadata"`
	}
	if err := json.Unmarshal(raw, &parsed); err != nil {
		return nil, fmt.Errorf("bad json from gemini: %w", err)
	}
	if len(parsed.Candidates) == 0 {
		return nil, fmt.Errorf("gemini: no candidates")
	}
	out := ""
	for _, pt := range parsed.Candidates[0].Content.Parts {
		out += pt.Text
	}
	return &Response{
		Text:         out,
		Provider:     p.Name,
		Model:        p.Model,
		PromptTokens: parsed.UsageMetadata.PromptTokenCount,
		OutTokens:    parsed.UsageMetadata.CandidatesTokenCount,
	}, nil
}
