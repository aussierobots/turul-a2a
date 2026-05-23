// Per-agent Go interop probe for `remote-delegate-agent`.
//
// This client is third-party from A2A's perspective — it does not
// import any `turul-*` code. It speaks to the delegate; the delegate
// then forwards to the upstream `skill-manifest-ollama-agent`. Two
// A2A hops total. From this client's view the upstream is invisible;
// the proof that the chain reached the upstream is the offline-stub
// marker string in the returned artifact body.
//
// Target agent default: http://localhost:3016 (override with A2A_BASE_URL).
package main

import (
	"context"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/a2aproject/a2a-go/v2/a2a"
	"github.com/a2aproject/a2a-go/v2/a2aclient"
	"github.com/a2aproject/a2a-go/v2/a2aclient/agentcard"
)

const (
	defaultAgentBaseURL     = "http://localhost:3016"
	expectedProtocolVersion = "1.0"

	greetPayload = `{"user":{"name":"Ada"},"style":"formal"}`
)

func agentBaseURL() string {
	if v := os.Getenv("A2A_BASE_URL"); v != "" {
		return v
	}
	return defaultAgentBaseURL
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

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()

	baseURL := agentBaseURL()
	fmt.Printf("target: %s\n", baseURL)

	card, err := agentcard.DefaultResolver.Resolve(ctx, baseURL)
	if err != nil {
		return fmt.Errorf("resolve agent card from %s: %w", baseURL, err)
	}

	fmt.Println("--- Delegate AgentCard (what THIS client sees) ---")
	fmt.Printf("Name:    %s\n", card.Name)
	fmt.Printf("Version: %s\n", card.Version)
	fmt.Print("Skills:  ")
	for i, sk := range card.Skills {
		if i > 0 {
			fmt.Print(", ")
		}
		fmt.Print(sk.ID)
	}
	fmt.Println()

	client, err := a2aclient.NewFromCard(ctx, card)
	if err != nil {
		return fmt.Errorf("build client from card: %w", err)
	}
	defer client.Destroy()

	msg := a2a.NewMessage(a2a.MessageRoleUser, a2a.NewTextPart(greetPayload))

	fmt.Println("--- Send (chain: client → delegate → upstream) ---")
	fmt.Printf("payload: %s\n", greetPayload)

	result, err := client.SendMessage(ctx, &a2a.SendMessageRequest{Message: msg})
	if err != nil {
		return fmt.Errorf("send message: %w", err)
	}

	task, ok := result.(*a2a.Task)
	if !ok {
		return fmt.Errorf("expected *a2a.Task, got %T", result)
	}

	if task.Status.State != a2a.TaskStateCompleted {
		return fmt.Errorf("expected TASK_STATE_COMPLETED, got %s", task.Status.State)
	}

	var artifactBody string
	for _, art := range task.Artifacts {
		for _, p := range art.Parts {
			if text := p.Text(); text != "" {
				artifactBody = text
				break
			}
		}
		if artifactBody != "" {
			break
		}
	}
	fmt.Printf("artifact: %s\n", artifactBody)

	// The "offline stub" marker in the artifact body proves the chain
	// reached the upstream — this client only knows about the
	// delegate, but the artifact body came from the upstream agent's
	// offline-mode greeting handler.
	if !strings.Contains(artifactBody, "offline stub") {
		return fmt.Errorf(
			"artifact missing 'offline stub' marker — chain didn't reach the upstream offline-mode path: %s",
			artifactBody,
		)
	}
	if !strings.Contains(artifactBody, "Ada") {
		return fmt.Errorf("artifact missing caller name 'Ada': %s", artifactBody)
	}

	fmt.Println("=== OK: two-hop chain returned the upstream's artifact ===")
	return nil
}
