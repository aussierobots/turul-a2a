// Per-agent Go interop probe for `skill-dispatch-profile-agent`.
//
// Activates the skill-invocation dispatcher profile by sending the
// `A2A-Extensions: https://turul.dev/a2a/extensions/skill-invocation/v1`
// header and stamping the two reserved keys (`a2a.skillId`,
// `a2a.skillParams`) into `Message.Metadata`. Calls `echo_loud` and
// `reverse` and prints the artifact text returned by each.
//
// Target agent default: http://localhost:3015 (override with A2A_BASE_URL).
package main

import (
	"context"
	"fmt"
	"net/http"
	"os"
	"sync"
	"time"

	"github.com/a2aproject/a2a-go/v2/a2a"
	"github.com/a2aproject/a2a-go/v2/a2aclient"
	"github.com/a2aproject/a2a-go/v2/a2aclient/agentcard"
)

const (
	defaultAgentBaseURL     = "http://localhost:3015"
	expectedProtocolVersion = "1.0"

	skillInvocationProfileURI = "https://turul.dev/a2a/extensions/skill-invocation/v1"

	metaSkillID     = "a2a.skillId"
	metaSkillParams = "a2a.skillParams"
)

func agentBaseURL() string {
	if v := os.Getenv("A2A_BASE_URL"); v != "" {
		return v
	}
	return defaultAgentBaseURL
}

// headerCapturingTransport wraps an http.RoundTripper to record the
// `A2A-Extensions` response header for every JSON-RPC call. The SDK
// does not surface response headers, so we observe them at the HTTP
// layer to verify the agent's echo behaviour.
type headerCapturingTransport struct {
	base http.RoundTripper

	mu       sync.Mutex
	lastEcho []string
}

func (t *headerCapturingTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	resp, err := t.base.RoundTrip(req)
	if err == nil && resp != nil {
		t.mu.Lock()
		t.lastEcho = append([]string(nil), resp.Header.Values("A2A-Extensions")...)
		t.mu.Unlock()
	}
	return resp, err
}

func (t *headerCapturingTransport) snapshot() []string {
	t.mu.Lock()
	defer t.mu.Unlock()
	return append([]string(nil), t.lastEcho...)
}

func main() {
	if err := run(); err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}

func run() error {
	if string(a2a.Version) != expectedProtocolVersion {
		return fmt.Errorf(
			"a2a-go SDK protocol version drift: got %q, expected %q",
			string(a2a.Version), expectedProtocolVersion,
		)
	}
	fmt.Printf("a2a-go protocol version: %s\n", a2a.Version)

	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	baseURL := agentBaseURL()
	fmt.Printf("target: %s\n", baseURL)

	card, err := agentcard.DefaultResolver.Resolve(ctx, baseURL)
	if err != nil {
		return fmt.Errorf("resolve agent card from %s: %w", baseURL, err)
	}
	fmt.Println("--- AgentCard ---")
	fmt.Printf("Name:    %s\n", card.Name)
	fmt.Printf("Version: %s\n", card.Version)
	fmt.Println("  Advertised extensions:")
	for _, ext := range card.Capabilities.Extensions {
		fmt.Printf("    - %s (required=%t)\n", ext.URI, ext.Required)
	}

	// Wrap the SDK's HTTP client so we can observe response headers
	// (the SDK exposes request-header injection via ServiceParams but
	// not response-header echo).
	capture := &headerCapturingTransport{base: http.DefaultTransport}
	httpClient := &http.Client{
		Timeout:   30 * time.Second,
		Transport: capture,
	}

	client, err := a2aclient.NewFromCard(
		ctx, card,
		a2aclient.WithJSONRPCTransport(httpClient),
	)
	if err != nil {
		return fmt.Errorf("build client from card: %w", err)
	}
	defer client.Destroy()

	// Activate the skill-invocation profile for every outgoing call by
	// attaching the A2A-Extensions service parameter to the context.
	// The JSON-RPC transport serialises ServiceParams as HTTP headers.
	ctx = a2aclient.AttachServiceParams(ctx, a2aclient.ServiceParams{
		"A2A-Extensions": {skillInvocationProfileURI},
	})

	if err := callSkill(ctx, client, capture, "echo_loud", map[string]any{"text": "hello"}); err != nil {
		return err
	}
	if err := callSkill(ctx, client, capture, "reverse", map[string]any{"text": "abc"}); err != nil {
		return err
	}

	return nil
}

func callSkill(
	ctx context.Context,
	client *a2aclient.Client,
	capture *headerCapturingTransport,
	skillID string,
	params map[string]any,
) error {
	msg := a2a.NewMessage(
		a2a.MessageRoleUser,
		a2a.NewTextPart(fmt.Sprintf("dispatch:%s", skillID)),
	)
	// Message.Metadata is `map[string]any` in a2a-go v2; the JSON
	// marshaller serialises it under the wire-level `metadata` key.
	msg.Metadata = map[string]any{
		metaSkillID:     skillID,
		metaSkillParams: params,
	}

	fmt.Printf("--- SendMessage request (skill=%s) ---\n", skillID)
	fmt.Printf("metadata: {%s=%q, %s=%v}\n", metaSkillID, skillID, metaSkillParams, params)

	result, err := client.SendMessage(ctx, &a2a.SendMessageRequest{Message: msg})
	if err != nil {
		return fmt.Errorf("send message (%s): %w", skillID, err)
	}

	task, ok := result.(*a2a.Task)
	if !ok {
		return fmt.Errorf("expected *a2a.Task, got %T", result)
	}

	fmt.Printf("--- SendMessage response (skill=%s) ---\n", skillID)
	fmt.Printf("kind=Task id=%s state=%s\n", task.ID, task.Status.State)
	if task.Status.State != a2a.TaskStateCompleted {
		return fmt.Errorf("skill %s: expected TASK_STATE_COMPLETED, got %s", skillID, task.Status.State)
	}
	for i, art := range task.Artifacts {
		fmt.Printf("  artifact[%d] id=%s name=%q parts=%d\n", i, art.ID, art.Name, len(art.Parts))
		for j, p := range art.Parts {
			fmt.Printf("    part[%d].text=%s\n", j, p.Text())
		}
	}
	echoed := capture.snapshot()
	fmt.Printf("response A2A-Extensions echo: %v\n", echoed)

	return nil
}
