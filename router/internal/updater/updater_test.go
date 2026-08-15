package updater

import "testing"

func TestHTTPSOnly(t *testing.T) {
	ok := []string{"https://example.com/m.json", " https://a.b/c "}
	bad := []string{
		"http://example.com/m.json", // downgrade
		"file:///C:/evil.exe",
		"ftp://example.com/x",
		"https://", // без хоста
		"",
		"://",
	}
	for _, u := range ok {
		if err := httpsOnly(u); err != nil {
			t.Errorf("httpsOnly(%q) = %v, ждали nil", u, err)
		}
	}
	for _, u := range bad {
		if err := httpsOnly(u); err == nil {
			t.Errorf("httpsOnly(%q) = nil, ждали ошибку", u)
		}
	}
}
