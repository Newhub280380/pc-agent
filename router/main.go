// Command router — сетевой слой агента: LLM Router, логирование, автообновление.
// Собирается в один статический .exe: `CGO_ENABLED=0 go build -o pcagent-router.exe`.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"github.com/newhub280380/pc-agent/router/internal/config"
	"github.com/newhub280380/pc-agent/router/internal/llm"
	"github.com/newhub280380/pc-agent/router/internal/server"
	"github.com/newhub280380/pc-agent/router/internal/updater"
)

// Version подставляется линкером: -ldflags "-X main.Version=1.0.3".
var Version = "dev"

func main() {
	envPath := flag.String("env", ".env", "путь к .env с ключами API")
	printVersion := flag.Bool("version", false, "показать версию и выйти")
	flag.Parse()

	if *printVersion {
		fmt.Println(Version)
		return
	}

	cfg, err := config.Load(*envPath)
	if err != nil {
		fmt.Fprintln(os.Stderr, "не читается .env:", err)
		os.Exit(1)
	}

	log := newLogger(cfg.LogDir)
	log.Info("router start", "version", Version, "listen", cfg.Listen, "providers", len(cfg.Providers))
	log.Info("Loading LLM config from", "env", *envPath)
	for _, p := range cfg.Providers {
		// Ключи НИКОГДА не логируем — только факт наличия и адрес.
		log.Info("provider", "name", p.Name, "model", p.Model, "base_url", p.BaseURL,
			"key", p.APIKey != "", "vision", p.Vision, "priority", p.Priority)
	}
	if len(cfg.Providers) == 0 {
		log.Warn("нет ни одного ключа — агент не сможет думать; заполни .env")
	}

	r := llm.New(cfg, log)
	srv := &http.Server{
		Addr:              cfg.Listen,
		Handler:           server.New(cfg, r, log).Handler(),
		ReadHeaderTimeout: 10 * time.Second,
	}

	// Автообновление: проверяем манифест, качаем новый бинарь рядом и
	// сигналим ядру. Само переключение делает ядро при рестарте —
	// подменять работающий .exe на Windows нельзя.
	go updater.Run(context.Background(), Version, log)

	go func() {
		if err := srv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Error("http server died", "err", err)
			os.Exit(1)
		}
	}()

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, os.Interrupt, syscall.SIGTERM)
	<-stop
	log.Info("router stop")
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	_ = srv.Shutdown(ctx)
}

// newLogger пишет одновременно в stdout (ядро показывает это в GUI-логе)
// и в файл (постмортем-разбор после падения).
func newLogger(dir string) *slog.Logger {
	var w io.Writer = os.Stdout
	if err := os.MkdirAll(dir, 0o755); err == nil {
		name := filepath.Join(dir, "router-"+time.Now().Format("2006-01-02")+".log")
		if f, err := os.OpenFile(name, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o644); err == nil {
			w = io.MultiWriter(os.Stdout, f)
		}
	}
	return slog.New(slog.NewJSONHandler(w, &slog.HandlerOptions{Level: slog.LevelInfo}))
}
