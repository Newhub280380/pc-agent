// Package llm: нейтральный формат запроса/ответа + адаптеры под вендоров.
//
// Зачем нейтральный формат: Rust-ядро вообще не должно знать, кто отвечает —
// GPT, Claude или локальная Qwen. Смена провайдера = смена строки в .env.
package llm

// Role — роль сообщения в диалоге.
type Role string

const (
	RoleSystem    Role = "system"
	RoleUser      Role = "user"
	RoleAssistant Role = "assistant"
)

// Message — одно сообщение. Image — PNG/JPEG в base64 (без data:-префикса).
// Зачем картинки прямо в сообщении: агент видит экран, и скриншот — это
// основной вход для «мышления». Текстовый OCR идёт отдельным полем Text.
type Message struct {
	Role     Role     `json:"role"`
	Text     string   `json:"text"`
	Images   []string `json:"images,omitempty"`
	ImageMIM string   `json:"image_mime,omitempty"` // по умолчанию image/png
}

// Request — что просит ядро.
type Request struct {
	Messages     []Message `json:"messages"`
	Temperature  float64   `json:"temperature"`
	MaxTokens    int       `json:"max_tokens"`
	JSONMode     bool      `json:"json_mode"`     // просим строгий JSON (план/действие)
	NeedVision   bool      `json:"need_vision"`   // отсечь провайдеров без картинок
	ForceProvide string    `json:"provider"`      // жёстко выбрать провайдера (отладка)
	Purpose      string    `json:"purpose"`       // plan|act|reflect|summarize — для логов и метрик
}

// Response — что возвращает роутер.
type Response struct {
	Text         string `json:"text"`
	Provider     string `json:"provider"`
	Model        string `json:"model"`
	LatencyMS    int64  `json:"latency_ms"`
	PromptTokens int    `json:"prompt_tokens"`
	OutTokens    int    `json:"output_tokens"`
	Attempts     int    `json:"attempts"`
	Error        string `json:"error,omitempty"`
}
