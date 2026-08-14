// Package configclient — Defing Go SDK。
// 端点池 failover：连接失败切换下一个端点（指数退避）。
package configclient

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
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
	http      *http.Client
}

func New(endpoints []string) *Client {
	return &Client{endpoints: endpoints, http: &http.Client{Timeout: 5 * time.Second}}
}

func (c *Client) request(path string) ([]byte, error) {
	var lastErr error
	for i, ep := range c.endpoints {
		resp, err := c.http.Get(ep + path)
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
			return nil, fmt.Errorf("GET %s -> %d", path, resp.StatusCode)
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

// Watch 订阅 (项目, 分支) 的发布事件；阻塞直至 stop 关闭；断线自动重连。
func (c *Client) Watch(project, branch string, listener func(WatchEvent), stop <-chan struct{}) error {
	path := fmt.Sprintf("/v1/projects/%s/branches/%s/watch", project, branch)
	attempt := 0
	for {
		if attempt > 0 {
			select {
			case <-stop:
				return nil
			case <-time.After(time.Duration(min(1000*(1<<attempt), 15000)) * time.Millisecond):
			}
		}
		attempt++
		resp, err := c.http.Get(c.endpoints[0] + path)
		if err != nil {
			continue
		}
		sc := bufio.NewScanner(resp.Body)
		for sc.Scan() {
			line := strings.TrimSpace(sc.Text())
			if strings.HasPrefix(line, "data:") {
				var e WatchEvent
				if err := json.Unmarshal([]byte(strings.TrimSpace(line[5:])), &e); err == nil {
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
