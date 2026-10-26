package llm

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/newhub280380/pc-agent/router/internal/config"
)

func TestEmbedKeepsOrderByIndex(t *testing.T) {
	var gotPath, gotModel string
	var gotInput []string
	s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotPath = r.URL.Path
		var body struct {
			Model string   `json:"model"`
			Input []string `json:"input"`
		}
		_ = json.NewDecoder(r.Body).Decode(&body)
		gotModel, gotInput = body.Model, body.Input
		w.Header().Set("Content-Type", "application/json")
		// Сервис вправе отдать векторы не по порядку — разбирать надо по index.
		_, _ = io.WriteString(w, `{"data":[{"index":1,"embedding":[0,1]},{"index":0,"embedding":[1,0]}]}`)
	}))
	defer s.Close()

	r := New(&config.Config{
		TimeoutSec: 5,
		Embed:      config.Embedder{BaseURL: s.URL, APIKey: "k", Model: "emb"},
	}, quietLogger())

	resp, err := r.Embed(context.Background(), EmbedRequest{Texts: []string{"первый", "второй"}})
	if err != nil {
		t.Fatalf("векторизация не прошла: %v", err)
	}
	if gotPath != "/embeddings" || gotModel != "emb" || len(gotInput) != 2 {
		t.Fatalf("запрос ушёл не туда: path=%s model=%s input=%v", gotPath, gotModel, gotInput)
	}
	if resp.Dim != 2 || resp.Vectors[0][0] != 1 || resp.Vectors[1][1] != 1 {
		t.Fatalf("векторы перепутаны: %+v", resp.Vectors)
	}
}

func TestEmbedWithoutEmbedderIsExplicit(t *testing.T) {
	r := New(&config.Config{TimeoutSec: 5}, quietLogger())
	_, err := r.Embed(context.Background(), EmbedRequest{Texts: []string{"x"}})
	if !errors.Is(err, ErrNoEmbedder) {
		t.Fatalf("без эмбеддера нужна ErrNoEmbedder, получено: %v", err)
	}
}

func TestEmbedRejectsPartialAnswer(t *testing.T) {
	s := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		_, _ = io.WriteString(w, `{"data":[{"index":0,"embedding":[1,0]}]}`)
	}))
	defer s.Close()
	r := New(&config.Config{
		TimeoutSec: 5,
		Embed:      config.Embedder{BaseURL: s.URL, Model: "emb"},
	}, quietLogger())
	if _, err := r.Embed(context.Background(), EmbedRequest{Texts: []string{"a", "b"}}); err == nil {
		t.Fatal("ответ с нехваткой векторов нельзя принимать: тексты съедут по индексам")
	}
}
