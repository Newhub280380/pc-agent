package llm

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
)

// EmbedRequest — что просит ядро: набор текстов на векторизацию.
type EmbedRequest struct {
	Texts []string `json:"texts"`
}

// EmbedResponse — векторы в том же порядке, что и тексты.
type EmbedResponse struct {
	Vectors [][]float32 `json:"vectors"`
	Model   string      `json:"model"`
	Dim     int         `json:"dim"`
	Error   string      `json:"error,omitempty"`
}

// ErrNoEmbedder — эмбеддер не настроен. Отдельная ошибка, а не текст: ядро по
// ней молча остаётся на полнотекстовом поиске, вместо того чтобы считать
// память сломанной.
var ErrNoEmbedder = errors.New("эмбеддер не настроен: задай EMBED_BASE_URL/EMBED_MODEL или ключ OpenAI-совместимого сервиса с /embeddings")

// Embed векторизует тексты одним запросом к OpenAI-совместимому /embeddings.
//
// Почему не через обычный Complete и не через весь список провайдеров:
// эмбеддинги нельзя мешать между моделями — векторы разных моделей
// несопоставимы, и фолбэк на «другого провайдера» дал бы мусорное сходство.
// Поэтому эмбеддер один, явно заданный, и при его отсутствии честный отказ.
func (r *Router) Embed(ctx context.Context, req EmbedRequest) (*EmbedResponse, error) {
	e := r.cfg.Embed
	if e.BaseURL == "" || e.Model == "" {
		return nil, ErrNoEmbedder
	}
	if len(req.Texts) == 0 {
		return &EmbedResponse{Vectors: [][]float32{}, Model: e.Model}, nil
	}
	body, err := json.Marshal(map[string]any{"model": e.Model, "input": req.Texts})
	if err != nil {
		return nil, err
	}
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodPost, e.BaseURL+"/embeddings", bytes.NewReader(body))
	if err != nil {
		return nil, err
	}
	httpReq.Header.Set("Content-Type", "application/json")
	if e.APIKey != "" {
		httpReq.Header.Set("Authorization", "Bearer "+e.APIKey)
	}
	resp, err := r.http.Do(httpReq)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(io.LimitReader(resp.Body, 64<<20))
	if err != nil {
		return nil, err
	}
	if resp.StatusCode >= 400 {
		return nil, &HTTPError{Code: resp.StatusCode, Body: string(raw)}
	}
	var parsed struct {
		Data []struct {
			Index     int       `json:"index"`
			Embedding []float32 `json:"embedding"`
		} `json:"data"`
	}
	if err := json.Unmarshal(raw, &parsed); err != nil {
		return nil, fmt.Errorf("ответ /embeddings не разобран: %w", err)
	}
	if len(parsed.Data) != len(req.Texts) {
		return nil, fmt.Errorf("сервис вернул %d векторов на %d текстов", len(parsed.Data), len(req.Texts))
	}
	out := make([][]float32, len(req.Texts))
	for _, d := range parsed.Data {
		idx := d.Index
		if idx < 0 || idx >= len(out) {
			return nil, fmt.Errorf("индекс вектора вне диапазона: %d", idx)
		}
		out[idx] = d.Embedding
	}
	dim := 0
	for _, v := range out {
		if len(v) == 0 {
			return nil, errors.New("сервис вернул пустой вектор")
		}
		if dim == 0 {
			dim = len(v)
		} else if len(v) != dim {
			return nil, fmt.Errorf("разная размерность векторов: %d и %d", dim, len(v))
		}
	}
	r.log.Info("embed ok", "model", e.Model, "texts", len(req.Texts), "dim", dim)
	return &EmbedResponse{Vectors: out, Model: e.Model, Dim: dim}, nil
}
