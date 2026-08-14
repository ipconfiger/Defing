// dsh 基准（A3）：读 QPS / 写 QPS 冒烟。用法: go run main.go -base http://127.0.0.1:8384 -read-n 20000 -read-c 200 -token <t>
// 仅标准库（与 Go SDK 同约束）。读：GET /v1/.../snapshot（数据面无鉴权）；写：POST /api/v1/.../publish（Bearer）。
package main

import (
	"bytes"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net/http"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

var client = &http.Client{Transport: &http.Transport{MaxIdleConnsPerHost: 512}}

func main() {
	base := flag.String("base", "http://127.0.0.1:8384", "dev-single base url")
	readN := flag.Int("read-n", 20000, "read requests")
	readC := flag.Int("read-c", 200, "read concurrency")
	writeN := flag.Int("write-n", 2000, "write requests")
	writeC := flag.Int("write-c", 50, "write concurrency")
	token := flag.String("token", "", "admin token (for write bench)")
	flag.Parse()

	snap := *base + "/v1/projects/bench-proj/branches/dev/snapshot"
	readQPS := bench(func(i int) {
		resp, err := client.Get(snap)
		if err != nil {
			return
		}
		io.Copy(io.Discard, resp.Body)
		resp.Body.Close()
	}, *readN, *readC)

	// 2) 写基准：每轮先 PUT 草稿（保持有草稿可发布），再 POST publish（单写者串行 apply 是瓶颈）
	var writeQPS float64
	if *token != "" && *writeN > 0 {
		writeQPS = writeBench(*base, *token, *writeN, *writeC)
	}

	fmt.Printf("READ_QPS=%.0f (n=%d c=%d)\n", readQPS, *readN, *readC)
	fmt.Printf("WRITE_QPS=%.0f (publish ok)\n", writeQPS)
}

func bench(fn func(int), n, c int) float64 {
	var wg sync.WaitGroup
	var done atomic.Int64
	start := time.Now()
	ch := make(chan int, n)
	for i := 0; i < n; i++ {
		ch <- i
	}
	close(ch)
	for w := 0; w < c; w++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := range ch {
				fn(i)
				done.Add(1)
			}
		}()
	}
	wg.Wait()
	el := time.Since(start).Seconds()
	return float64(done.Load()) / el
}

var _ = strings.TrimSpace

func writeBench(base, token string, n, c int) float64 {
	draftURL := base + "/api/v1/projects/bench-proj/branches/dev/draft"
	pubURL := base + "/api/v1/projects/bench-proj/branches/dev/publish"
	var wg sync.WaitGroup
	var ok atomic.Int64
	start := time.Now()
	for w := 0; w < c; w++ {
		wg.Add(1)
		go func(w int) {
			defer wg.Done()
			for i := 0; i < n/c; i++ {
				rid := fmt.Sprintf("bench-%d-%d", time.Now().UnixNano(), i)
				draftBody, _ := json.Marshal(map[string]any{
					"updates": []map[string]any{{
						"group": "g", "key": "k",
						"value": map[string]string{"type": "string", "str_value": "v"},
					}},
				})
				req, _ := http.NewRequest("PUT", draftURL, bytes.NewReader(draftBody))
				req.Header.Set("Content-Type", "application/json")
				req.Header.Set("Authorization", "Bearer "+token)
				resp, err := client.Do(req)
				if err == nil {
					io.Copy(io.Discard, resp.Body)
					resp.Body.Close()
				}
				body, _ := json.Marshal(map[string]string{"comment": "bench", "request_id": rid})
				preq, _ := http.NewRequest("POST", pubURL, bytes.NewReader(body))
				preq.Header.Set("Content-Type", "application/json")
				preq.Header.Set("Authorization", "Bearer "+token)
				presp, err := client.Do(preq)
				if err != nil {
					continue
				}
				io.Copy(io.Discard, presp.Body)
				presp.Body.Close()
				if presp.StatusCode == 200 {
					ok.Add(1)
				}
			}
		}(w)
	}
	wg.Wait()
	el := time.Since(start).Seconds()
	return float64(ok.Load()) / el
}
