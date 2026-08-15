// Package updater: тихое автообновление бинарей агента.
//
// Как это работает и почему так:
//  1. Раз в час читаем JSON-манифест (UPDATE_MANIFEST_URL).
//  2. Если версия новее — качаем файл во временный, проверяем SHA-256.
//  3. Кладём рядом как *.new. Windows не даёт перезаписать запущенный .exe,
//     поэтому подмену делает лаунчер ядра при следующем старте.
//
// Альтернативы: MSI/Squirrel/WinSparkle — надёжнее для массового продукта,
// но требуют инсталлятора и подписи. У нас требование «1 файл, запустил и
// поехал», поэтому self-update своими руками.
package updater

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	neturl "net/url"
	"os"
	"path/filepath"
	"strings"
	"time"
)

type manifest struct {
	Version string `json:"version"`
	URL     string `json:"url"`
	SHA256  string `json:"sha256"`
	Notes   string `json:"notes"`
}

// Run — фоновый цикл. Никогда не паникует: обновление не должно ронять агента.
func Run(ctx context.Context, current string, log *slog.Logger) {
	url := strings.TrimSpace(os.Getenv("UPDATE_MANIFEST_URL"))
	if url == "" {
		log.Info("updater disabled (UPDATE_MANIFEST_URL пуст)")
		return
	}
	t := time.NewTicker(time.Hour)
	defer t.Stop()
	for {
		if err := checkOnce(ctx, url, current, log); err != nil {
			log.Warn("update check failed", "err", err)
		}
		select {
		case <-ctx.Done():
			return
		case <-t.C:
		}
	}
}

// httpsOnly запрещает и сам http, и downgrade через 302 на http: манифест
// решает, какой бинарь мы запустим, поэтому канал обязан быть TLS.
func httpsOnly(raw string) error {
	u, err := neturl.Parse(strings.TrimSpace(raw))
	if err != nil {
		return fmt.Errorf("некорректный URL обновления: %w", err)
	}
	if u.Scheme != "https" || u.Host == "" {
		return fmt.Errorf("обновления разрешены только по https, получено %q", raw)
	}
	return nil
}

func newClient() *http.Client {
	return &http.Client{
		Timeout: 30 * time.Second,
		CheckRedirect: func(req *http.Request, via []*http.Request) error {
			if len(via) >= 5 {
				return errors.New("слишком много редиректов")
			}
			return httpsOnly(req.URL.String())
		},
	}
}

func checkOnce(ctx context.Context, url, current string, log *slog.Logger) error {
	if err := httpsOnly(url); err != nil {
		return err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return err
	}
	hc := newClient()
	resp, err := hc.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("manifest http %d", resp.StatusCode)
	}
	var m manifest
	// Манифест — это десяток строк JSON; всё, что больше 1 МиБ, — мусор.
	if err := json.NewDecoder(io.LimitReader(resp.Body, 1<<20)).Decode(&m); err != nil {
		return err
	}
	if m.Version == "" || m.Version == current {
		return nil
	}
	if err := httpsOnly(m.URL); err != nil {
		return err
	}
	// Без валидного хеша проверять нечего, а качать бинарь «на доверии» нельзя.
	if len(m.SHA256) != 64 {
		return fmt.Errorf("в манифесте нет корректного sha256")
	}
	if _, err := hex.DecodeString(m.SHA256); err != nil {
		return fmt.Errorf("sha256 в манифесте не hex: %w", err)
	}
	log.Info("новая версия доступна", "from", current, "to", m.Version, "notes", m.Notes)

	self, err := os.Executable()
	if err != nil {
		return err
	}
	// Уникальное имя: два экземпляра роутера не должны писать в один файл.
	tmpf, err := os.CreateTemp(filepath.Dir(self), ".update-*.tmp")
	if err != nil {
		return err
	}
	tmp := tmpf.Name()
	tmpf.Close()
	defer os.Remove(tmp)
	if err := download(ctx, hc, m.URL, tmp); err != nil {
		return err
	}
	sum, err := sha256file(tmp)
	if err != nil {
		return err
	}
	if !strings.EqualFold(sum, m.SHA256) {
		// Несовпадение хеша = либо битая загрузка, либо подмена. Обновление
		// отменяется молча — лучше старая версия, чем чужой бинарь.
		return fmt.Errorf("sha256 mismatch: got %s want %s", sum, m.SHA256)
	}
	target := self + ".new"
	os.Remove(target)
	if err := os.Rename(tmp, target); err != nil {
		return err
	}
	log.Info("обновление скачано, применится при следующем запуске", "file", target)
	return nil
}

func download(ctx context.Context, hc *http.Client, url, dst string) error {
	if err := httpsOnly(url); err != nil {
		return err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return err
	}
	resp, err := hc.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("download http %d", resp.StatusCode)
	}
	f, err := os.OpenFile(dst, os.O_WRONLY|os.O_TRUNC, 0o755)
	if err != nil {
		return err
	}
	defer f.Close()
	// Лимит 256 МБ: защита от «бесконечного» ответа, который забьёт диск.
	// Читаем на байт больше, чтобы отличить обрезанный файл от ровно влезшего.
	const maxSize = 256 << 20
	n, err := io.Copy(f, io.LimitReader(resp.Body, maxSize+1))
	if err != nil {
		return err
	}
	if n > maxSize {
		return fmt.Errorf("файл обновления больше %d байт", maxSize)
	}
	return nil
}

func sha256file(path string) (string, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		return "", err
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}
