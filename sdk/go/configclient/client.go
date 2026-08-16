// Package configclient — Defing Go SDK。
// 端点池 failover：连接失败切换下一个端点（指数退避）。
package configclient

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"strings"
	"time"
)

type Change struct {
	Group    string `json:"group"`
	Key      string `json:"key"`
	Kind     string `json:"kind"`
	NewValue any    `json:"new_value,omitempty"`
}

type WatchEvent struct {
	Project          string   `json:"project"`
	Branch           string   `json:"branch"`
	Version          int64    `json:"version"`
	Ty               string   `json:"ty"`
	StructureVersion int64    `json:"structure_version"`
	Comment          string   `json:"comment"`
	RequestID        string   `json:"request_id"`
	Changes          []Change `json:"changes"`
	SnapshotRequired bool     `json:"snapshot_required,omitempty"`
}

// Member — 集群成员（gRPC ListMembers）。
type Member struct {
	NodeID         string
	GrpcAddr       string
	HTTPAddr       string
	IsLeader       bool
	IsVoter        bool
	CommittedIndex int64
}

type Snapshot struct {
	Project          string                    `json:"project"`
	Branch           string                    `json:"branch"`
	Version          int64                     `json:"version"`
	StructureVersion int64                     `json:"structure_version"`
	Groups           map[string]map[string]any `json:"groups"`
}

type Client struct {
	endpoints []string
	http      *http.Client // 普通请求：5s 超时
	sse       *http.Client // SSE 长连接：无整体超时（F-SDK：5s 整体超时会周期性掐断 watch）
	token     string       // 数据面令牌（D2：配置时 Authorization Bearer 携带）
}

// New 创建 HTTP 数据面客户端；opts[0] 为可选数据面令牌（--data-plane-token）。
func New(endpoints []string, opts ...string) *Client {
	// 普通请求 5s 超时
	httpClient := &http.Client{Timeout: 5 * time.Second}
	// SSE 长连接：仅连接超时，不设整体超时（断线由服务端 keepalive/连接错误触发，客户端用 after_version 续传）
	sseClient := &http.Client{
		Transport: &http.Transport{
			DialContext: (&net.Dialer{Timeout: 3 * time.Second}).DialContext,
		},
	}
	token := ""
	if len(opts) > 0 {
		token = opts[0]
	}
	return &Client{endpoints: endpoints, http: httpClient, sse: sseClient, token: token}
}

func (c *Client) authHeaders() http.Header {
	h := http.Header{}
	if c.token != "" {
		h.Set("Authorization", "Bearer "+c.token)
	}
	return h
}

func (c *Client) request(path string) ([]byte, error) {
	var lastErr error
	for i, ep := range c.endpoints {
		req, err := http.NewRequest(http.MethodGet, ep+path, nil)
		if err != nil {
			lastErr = err
			continue
		}
		req.Header = c.authHeaders()
		resp, err := c.http.Do(req)
		if err != nil {
			lastErr = err
			time.Sleep(time.Duration(100*(i+1)) * time.Millisecond)
			continue
		}
		body, err := io.ReadAll(resp.Body)
		resp.Body.Close()
		if err != nil {
			lastErr = err
			continue
		}
		if resp.StatusCode != http.StatusOK {
			return nil, fmt.Errorf("GET %s -> %d: %s", path, resp.StatusCode, strings.TrimSpace(string(body)))
		}
		return body, nil
	}
	return nil, fmt.Errorf("all endpoints unreachable: %w", lastErr)
}

func (c *Client) Get(project, branch string) (*Snapshot, error) {
	body, err := c.request(fmt.Sprintf("/v1/projects/%s/branches/%s/snapshot", project, branch))
	if err != nil {
		return nil, err
	}
	var s Snapshot
	if err := json.Unmarshal(body, &s); err != nil {
		return nil, err
	}
	return &s, nil
}

func (c *Client) GetItem(project, branch, group, key string) (any, error) {
	s, err := c.Get(project, branch)
	if err != nil {
		return nil, err
	}
	if g, ok := s.Groups[group]; ok {
		if v, ok := g[key]; ok {
			return v, nil
		}
	}
	return nil, nil
}

// Watch 订阅 (项目, 分支) 的发布事件；阻塞直至 stop 关闭；断线自动重连（after_version 续传）。
// F-SDK：① SSE 用无整体超时的独立 client（不再每 5s 被掐断）；② 事件按版本去重（重放/重连不重复回调）。
func (c *Client) Watch(project, branch string, listener func(WatchEvent), stop <-chan struct{}) error {
	path := fmt.Sprintf("/v1/projects/%s/branches/%s/watch", project, branch)
	attempt := 0
	var lastVersion int64
	for {
		if attempt > 0 {
			select {
			case <-stop:
				return nil
			case <-time.After(time.Duration(min(1000*(1<<attempt), 15000)) * time.Millisecond):
			}
		}
		attempt++
		resume := ""
		if lastVersion > 0 {
			resume = fmt.Sprintf("?after_version=%d", lastVersion)
		}
		resp, err := func() (*http.Response, error) {
			req, rerr := http.NewRequest(http.MethodGet, c.endpoints[0]+path+resume, nil)
			if rerr != nil {
				return nil, rerr
			}
			req.Header = c.authHeaders()
			return c.sse.Do(req)
		}()
		if err != nil {
			continue
		}
		sc := bufio.NewScanner(resp.Body)
		for sc.Scan() {
			line := strings.TrimSpace(sc.Text())
			if strings.HasPrefix(line, "data:") {
				var e WatchEvent
				if err := json.Unmarshal([]byte(strings.TrimSpace(line[5:])), &e); err == nil {
					if e.Version <= lastVersion {
						continue // 重放/重连重复投递 → 去重
					}
					lastVersion = e.Version
					listener(e)
				}
			}
		}
		resp.Body.Close()
		select {
		case <-stop:
			return nil
		default:
		}
	}
}
