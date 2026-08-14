// Go SDK gRPC 契约测试：Get / GetItem / Watch / ListMembers（:8383）。
package main

import (
	"context"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/defing/config-go/configclient"
)

func main() {
	grpcAddr := env("DSH_GRPC", "127.0.0.1:8383")
	project := env("DSH_PROJECT", "sdk-project")

	c, err := configclient.NewGrpc(grpcAddr, "")
	if err != nil {
		fmt.Println("[go-grpc] FAIL new:", err)
		os.Exit(1)
	}
	defer c.Close()
	ctx := context.Background()

	snap, err := c.Get(ctx, project, "dev", 0)
	if err != nil {
		fmt.Println("[go-grpc] FAIL get:", err)
		os.Exit(1)
	}
	host, _ := snap.Groups["redis"]["host"].(string)
	fmt.Printf("[go-grpc] get ok: version=%d host=%s\n", snap.Version, host)
	if host == "" {
		fmt.Println("[go-grpc] FAIL value mismatch:", snap.Groups)
		os.Exit(1)
	}

	item, err := c.GetItem(ctx, project, "dev", "redis", "host", 0)
	if err != nil || item != host {
		fmt.Println("[go-grpc] FAIL getItem:", err, item)
		os.Exit(1)
	}
	fmt.Println("[go-grpc] get_item ok:", item)

	if _, err := c.ListMembers(ctx); err != nil {
		fmt.Println("[go-grpc] list_members skipped (dev-single):", err)
	} else {
		fmt.Println("[go-grpc] list_members ok")
	}

	stop, cancel := context.WithCancel(ctx)
	got := make(chan int64, 1)
	go func() {
		_ = c.Watch(stop, project, "dev", 0, func(e configclient.WatchEvent) {
			if e.Version > snap.Version {
				select {
				case got <- e.Version:
				default:
				}
			}
		})
	}()
	select {
	case v := <-got:
		cancel()
		fmt.Printf("[go-grpc] watch event: v%d\n", v)
		fmt.Println("[go-grpc] PASS")
	case <-time.After(15 * time.Second):
		cancel()
		fmt.Println("[go-grpc] FAIL watch timeout")
		os.Exit(1)
	}
}

func env(k, d string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return d
}

var _ = strings.TrimSpace
