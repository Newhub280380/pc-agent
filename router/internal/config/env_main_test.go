// TestMain: изоляция тестов от реального окружения машины.
// На машине разработчика могут стоять живые ключи (например OPENROUTER_API_KEY),
// а Load читает не только .env, но и переменные процесса — без этой очистки
// тесты сравниваются с мусором из env и падают на ровном месте.
package config

import (
	"os"
	"testing"
)

func TestMain(m *testing.M) {
	for _, k := range []string{
		"OPENAI_API_KEY",
		"OPENAI_MODEL",
		"OPENAI_BASE_URL",
		"ANTHROPIC_API_KEY",
		"ANTHROPIC_MODEL",
		"ANTHROPIC_BASE_URL",
		"GEMINI_API_KEY",
		"GEMINI_MODEL",
		"GEMINI_BASE_URL",
		"DEEPSEEK_API_KEY",
		"DEEPSEEK_MODEL",
		"DEEPSEEK_BASE_URL",
		"XAI_API_KEY",
		"XAI_MODEL",
		"XAI_BASE_URL",
		"QWEN_API_KEY",
		"QWEN_MODEL",
		"QWEN_BASE_URL",
		"OPENROUTER_API_KEY",
		"OPENROUTER_MODEL",
		"OPENROUTER_BASE_URL",
		"YOUTORIA_API_KEY",
		"YOUTORIA_MODEL",
		"YOUTORIA_BASE_URL",
		"NVIDIA_API_KEY",
		"NVIDIA_MODEL",
		"NVIDIA_BASE_URL",
		"KILO_API_KEY",
		"KILO_MODEL",
		"KILO_BASE_URL",
		"LOCAL_API_KEY",
		"LOCAL_MODEL",
		"LOCAL_BASE_URL",
		"ROUTER_LISTEN",
		"ROUTER_TOKEN",
		"LOG_DIR",
		"LLM_MAX_RETRIES",
		"LLM_TIMEOUT_SEC",
		"LLM_PRIORITY",
		"LLM_PROVIDER",
		"LLM_BASE_URL",
		"LLM_API_KEY",
		"LLM_MODEL",
		} {
			os.Unsetenv(k)
		}
	os.Exit(m.Run())
}
