// Go SDK 契约测试：get + watch。
package main

import (
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/defing/config-go/configclient"
)

func main() {
	endpoints := strings.Split(env("DSH_ENDPOINTS", "http://127.0.0.1:8384"), ",")
	project := env("DSH_PROJECT", "sdk-project")
	c := configclient.New(endpoints)

	snap, err := c.Get(project, "dev")
	if err != nil {
		fmt.Println("[go] FAIL get:", err)
		os.Exit(1)
	}
	host, _ := snap.Groups["redis"]["host"].(string)
	fmt.Printf("[go] get ok: version=%d host=%s\n", snap.Version, host)
	if host == "" {
		fmt.Println("[go] FAIL value mismatch:", snap.Groups)
		os.Exit(1)
	}

	stop := make(chan struct{})
	got := make(chan int64, 1)
	go func() {
		_ = c.Watch(project, "dev", func(e configclient.WatchEvent) {
			if e.Version > snap.Version {
				got <- e.Version
			}
		}, stop)
	}()
	select {
	case v := <-got:
		fmt.Printf("[go] watch event: v%d\n", v)
	case <-time.After(10 * time.Second):
		fmt.Println("[go] FAIL watch timeout")
		close(stop)
		os.Exit(1)
	}
	close(stop)
	fmt.Println("[go] PASS")
}

func env(k, d string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return d
}
